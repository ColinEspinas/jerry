use super::*;
use crate::root::plural;
use crate::root::scrollbar;
use crate::root::widgets::{
    self, hover_keycap_row, menu_popover_chrome, render_env_chip, render_keycap_row, KeycapSize,
    SimpleInput, TextFieldHandle,
};
use crate::settings::widgets::ChoiceOption;
use crate::sound::SoundEventKind;

/// The Shell suggestion dropdown's width (GitHub issue #213's follow-up). Wider than the 168px
/// field it hangs under, because a row carries a shell's name *and* its real absolute path
/// (`/usr/local/bin/fish`) and truncating that away would defeat the point of showing the path at
/// all. Sized between the git graph's own two menus (`theme::graph::PUSH_MENU_WIDTH` 268,
/// `ROW_MENU_WIDTH` 330) rather than picked freehand.
const SHELL_SUGGESTIONS_WIDTH: gpui::Pixels = px(288.0);

/// How tall the row list may grow before it scrolls. Nine 29px rows plus the panel's own padding,
/// which comfortably clears what a real machine lists (this one's `/etc/shells` yields seven after
/// deduplication) while keeping the dropdown from ever covering the whole Settings page on a host
/// with an unusually long list. Anything beyond it is genuinely reachable by scrolling, not
/// silently dropped from the detection results.
const SHELL_SUGGESTIONS_MAX_HEIGHT: gpui::Pixels = px(269.0);

/// The smallest left offset the dropdown will accept, for a window narrow enough that
/// right-aligning it against the field would push it off the left edge. Mirrors
/// `crate::menu::model::MENU_EDGE_MARGIN`, whose job is exactly this.
const SHELL_SUGGESTIONS_EDGE_MARGIN: f32 = 4.0;

/// The sound-event picker dropdown's width (GitHub issue #226) - narrower than the Shell
/// suggestion dropdown's 288px, since a row here shows only a sound's plain display name, never
/// a path.
const SOUND_PICKER_WIDTH: gpui::Pixels = px(220.0);

/// Same "scrolls past this many rows rather than covering the whole page" reasoning as
/// [`SHELL_SUGGESTIONS_MAX_HEIGHT`] - comfortably more than the built-in library alone, still
/// bounded once the user has imported several sounds.
const SOUND_PICKER_MAX_HEIGHT: gpui::Pixels = px(232.0);

/// Same role as [`SHELL_SUGGESTIONS_EDGE_MARGIN`], for the sound picker.
const SOUND_PICKER_EDGE_MARGIN: f32 = 4.0;

/// The opacity a [`SoundEventKind`] row is dimmed to while the master "Sound effects" switch
/// (`crate::settings::store::SoundSettings::enabled`) is off - `Self::render_sound_event_row`'s
/// visual half of "this row currently has no effect", paired with
/// `Self::render_toggle_control_gated`/`Self::render_sound_picker_trigger`'s `interactive: false`
/// for the behavioural half. `gpui`'s `opacity` dims an element's whole subtree in one call
/// (`vendor/zed/crates/gpui/src/elements/div.rs`'s `window.with_element_opacity`), so this single
/// wrapper covers the row's label, hint, sound-choice trigger, and toggle together rather than
/// needing a disabled variant of each one's own colors.
const SOUND_ROW_DISABLED_OPACITY: f32 = 0.4;

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
        if page != SettingsPage::Notifications {
            // Same "leaving the page closes its own scoped popover" discipline as the Theme
            // remove-arm just above - `Self::render_sound_picker`'s own `.when` already gates on
            // the current page, so this is about not showing it *re-opened* the moment the user
            // navigates back, not about anything painting while away.
            self.sound_picker_open = None;
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
                    .h(theme::band::CHROME_HEADER)
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
                        // GitHub issue #128.
                        hover_keycap_row(div().id("settings-close").cursor_pointer())
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
                    .children(scrollbar::render_vertical_scrollbar(
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
                                        SettingsPage::Notifications => self
                                            .render_settings_notifications_page(cx)
                                            .into_any_element(),
                                        _ => render_settings_placeholder_page().into_any_element(),
                                    }),
                            ),
                    )
                    .children(scrollbar::render_vertical_scrollbar(
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
        // `row.kind` is an `AgentKind` (this card only ever lists real agent CLIs); the badge
        // helpers are shared with the tab strip/rail, which also have to draw shells, so they
        // take the wider `ProcessKind`.
        let badge_kind = ProcessKind::from(row.kind);
        let (badge_fg, badge_bg) = work_surface::agent_tint(badge_kind);
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
                    .child(work_surface::agent_initial(badge_kind)),
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
    /// and inert (no `on_click`) - `crate::work_surface::agents::AgentKind` is a fixed Rust enum,
    /// so there is no runtime "register a new agent binary" flow to wire this to yet.
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
                                    .child(crate::rail::state::worktree_disk_label(
                                        worktree_count,
                                        &disk_label,
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
        let shell_row = self.render_settings_row(
            "Shell",
            "What a new Shell tab runs - click the field for the shells detected on this machine, \
             or type any name on PATH or absolute path. Leave it empty to use the system default. \
             Agent tabs are unaffected.",
            self.render_settings_shell_control(cx),
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
            .child(shell_row)
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

    /// GitHub issue #213's "Shell" control: a real, focusable free-text field naming the program
    /// a Shell tab launches, plus a live, advisory hint saying what that name really resolves to
    /// right now ([`Self::shell_status`]).
    ///
    /// The field is the same minimal hand-rolled input shape as the Themes page's seed field
    /// ([`Self::render_theme_seed_row`]) and the Keybindings filter - a real `FocusHandle`, a
    /// real caret ([`Self::render_simple_input_caret`]), append/backspace/`Esc`-clears, real
    /// per-widget undo - deliberately reusing that established pattern rather than introducing a
    /// second, richer text-input mechanism this app doesn't otherwise have.
    ///
    /// The placeholder is the real answer to "what happens if I leave this blank": whichever
    /// program the OS itself names, not a blank field with no consequence stated.
    fn render_settings_shell_control(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shell = self.shell_input.as_str().to_string();
        let has_shell = !shell.is_empty();
        // The real program an empty field means on *this* machine, read live rather than
        // described as a generic "$SHELL" - see `TerminalSpec::default_shell_program_display`.
        let placeholder = crate::terminal::pane::TerminalSpec::default_shell_program_display();

        div()
            .flex()
            .items_center()
            .gap(px(9.0))
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(if self.shell_status.is_not_found() {
                        theme::status::FAIL
                    } else {
                        theme::text::FAINTER
                    })
                    .child(self.shell_status.hint())
                    .debug_selector(|| "settings-shell-status".to_string()),
            )
            .child(
                self.wire_text_input_actions(
                    div()
                        .id("settings-shell-input")
                        .debug_selector(|| "settings-shell-input".to_string())
                        .track_focus(&self.shell_focus_handle)
                        // See `crate::default_key_bindings`' `TextUndo`/`TextRedo` docs for why
                        // the tag and the listeners both live on this exact node.
                        .key_context("text-input")
                        .on_action(cx.listener(Self::handle_settings_shell_text_undo))
                        .on_action(cx.listener(Self::handle_settings_shell_text_redo))
                        .on_key_down(cx.listener(Self::handle_settings_shell_key_down)),
                    shell_input_handle(),
                    cx,
                )
                    .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        window.focus(&this.shell_focus_handle, cx);
                        this.open_shell_suggestions(cx);
                    }))
                    // The field's real, window-space painted bounds, for positioning the
                    // suggestion dropdown - the same `gpui::canvas` idiom
                    // `Self::plus_button_bounds` uses, and for the same reason: the dropdown is a
                    // top-level sibling in `AdeApp::render`, so it needs the field's position in
                    // window space, not in this row's own coordinate system.
                    .child({
                        let this = cx.entity();
                        gpui::canvas(
                            move |bounds, _window, cx| {
                                this.update(cx, |this, _cx| {
                                    this.shell_field_bounds = bounds;
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full()
                    })
                    .cursor_pointer()
                    .flex_none()
                    .flex()
                    .items_center()
                    // No decorative gap before the caret - see
                    // `crate::rail::render::AdeApp::render_rail_filter_row`'s own
                    // comment for why (live report: it read as a gap between the
                    // typed text and where it's actually being typed).
                    .h(px(20.0))
                    .w(px(168.0))
                    .px(px(7.0))
                    .rounded(theme::radius::BUTTON)
                    .border_1()
                    .border_color(theme::border::CARD_FIELD)
                    .bg(theme::surface::CARD_SUNK)
                    // Caret placement and text sizing both through
                    // `AdeApp::render_simple_input_row`, which owns that structure for every
                    // simple input in this app. This field was the *second* live instance of the
                    // bug that helper exists to make unrepeatable: `.flex_1().min_w_0()` sat on
                    // the text element, so inside this fixed 168px box the text's layout box
                    // filled the whole field whatever the shell path said, and the caret after it
                    // sat pinned to the right-hand border instead of against the last character.
                    .child(self.render_simple_input_row(
                        SimpleInput {
                            caret_selector: "settings-shell-caret".into(),
                            text_selector: "settings-shell-text".into(),
                            focus_handle: Some(&self.shell_focus_handle),
                            text: if has_shell { &shell } else { "" },
                            caret_offset: self.shell_input.caret(),
                            selection: self.shell_input.selection(),
                            placeholder: &placeholder,
                            font: theme::font::MONO,
                            text_size: self.ui_text_size(10.5),
                            text_color: theme::text::BODY,
                            placeholder_color: theme::text::GHOST,
                            field: Some(shell_input_handle()),
                        },
                        cx,
                    )),
            )
    }

    /// Same minimal append/backspace/`Esc`-clears shape as
    /// [`Self::handle_settings_keymap_filter_key_down`] - see that method's docs for the
    /// deliberate scope cut (no cursor positioning, no selection, no IME). Every real change
    /// goes straight to the persisted setting through [`Self::apply_shell_input`], so there is
    /// no separate "save" step that could be forgotten.
    ///
    /// The suggestion dropdown deliberately consumes **no** keystroke this handler needs. It has
    /// no keyboard selection of its own (no up/down/enter capture), so every key still means
    /// exactly what it meant before the dropdown existed - the field is the only thing typing can
    /// reach. `Esc` in particular keeps its existing, tested meaning (clear the field, undoably),
    /// rather than being stolen as a "close the dropdown" key; it just closes the dropdown as
    /// well, since a cleared field is the user saying they want out of the way, and every other
    /// edit re-opens it so the filtered list keeps up with what is being typed.
    pub(in crate::settings) fn handle_settings_shell_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // GitHub issue #336: `widgets::text_editing_modifiers` rather than a flat "any modifier
        // means not ours" - see `crate::rail::render::AdeApp::handle_filter_key_down`'s own note.
        let Some(modifiers) = widgets::text_editing_modifiers(&keystroke.key, &keystroke.modifiers)
        else {
            return;
        };
        self.reset_caret_blink(cx);
        let escaped = keystroke.key.as_str() == "escape";
        let changed = match keystroke.key.as_str() {
            // Clearing the field is itself a real, meaningful edit here (it means "go back to the
            // system default"), so it persists like any other - and is undoable, like every other
            // simple input's `Esc`.
            "escape" => self.shell_input.clear(Instant::now()),
            // GitHub issue #336: the whole `TextField` vocabulary rather than the
            // backspace/insert half this used to hand-roll - caret movement, selection extension,
            // word-wise movement and Delete all arrive here at once, for this field and the two
            // below it.
            key => {
                self.shell_input
                    .handle_editing_key(key, keystroke.key_char.as_deref(), modifiers, Instant::now())
            }
        };
        if changed {
            self.apply_shell_input(cx);
            cx.stop_propagation();
        }
        if escaped {
            self.shell_suggestions_open = false;
            cx.notify();
        } else if changed {
            self.open_shell_suggestions(cx);
        }
    }

    /// Opens (or re-opens) the Shell field's suggestion dropdown, re-detecting the machine's real
    /// shells first (GitHub issue #213's follow-up).
    ///
    /// The detection runs here, on a real user gesture - a click on the field, a keystroke that
    /// changed it - and never from `render`: [`crate::settings::state::detect_installed_shells`]
    /// reads `/etc/shells` and walks `$PATH`, which is exactly the class of work
    /// [`Self::refresh_shell_status`] and [`Self::load_agent_rows`] already keep off the frame
    /// path. Re-running it per gesture rather than once at startup is what makes a shell the user
    /// installed *while* the app was running actually show up.
    ///
    /// Goes through [`Self::close_menu_surfaces_except`] like every other menu-opening path
    /// (GitHub issue #176), so this dropdown and some other popover can never be painted at once.
    pub(in crate::settings) fn open_shell_suggestions(&mut self, cx: &mut Context<Self>) {
        let _ = self.close_menu_surfaces_except(Some(menus::MenuSurface::ShellSuggestions));
        self.refresh_shell_suggestions();
        self.shell_suggestions_open = true;
        cx.notify();
    }

    /// Re-detects the shells this machine genuinely has ([`Self::shell_suggestions`]). Separate
    /// from [`Self::open_shell_suggestions`] so opening Settings can warm it without also opening
    /// the dropdown.
    pub(crate) fn refresh_shell_suggestions(&mut self) {
        self.shell_suggestions = settings::detect_installed_shells();
    }

    /// Puts a clicked suggestion's real path into the field, exactly as if it had been typed:
    /// same [`text_history::TextField`] (so it is a single undoable edit, and the field's existing
    /// caret/undo behaviour is untouched), same [`Self::apply_shell_input`] persistence path, same
    /// advisory status hint recomputed afterwards. Nothing about a suggested value is special once
    /// it is in the field - which is the whole point of the field staying free text.
    pub(in crate::settings) fn select_shell_suggestion(
        &mut self,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shell_input.set(&value, Instant::now());
        self.apply_shell_input(cx);
        self.shell_suggestions_open = false;
        // Focus goes back to the field, not the dropdown that just vanished, so the very next
        // keystroke edits the value the user just picked.
        window.focus(&self.shell_focus_handle, cx);
        cx.notify();
    }

    /// `TextUndo`/`TextRedo` for the Shell field (GitHub issue #17's per-widget undo) - see
    /// `crate::default_key_bindings`' own docs for the scoping. Both re-apply the resulting text
    /// to the real setting, so an undo can't leave the field and the file disagreeing.
    pub(in crate::settings) fn handle_settings_shell_text_undo(
        &mut self,
        _: &TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shell_input.undo() {
            self.apply_shell_input(cx);
        }
    }

    pub(in crate::settings) fn handle_settings_shell_text_redo(
        &mut self,
        _: &TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shell_input.redo() {
            self.apply_shell_input(cx);
        }
    }

    /// Copies the Shell field's current text into the real, persisted setting and saves it
    /// (GitHub issue #213). An empty/whitespace-only field is stored as a real `None` - "use the
    /// system default" - never as `Some("")`.
    ///
    /// Deliberately does **not** touch already-open tabs: which program a terminal runs is fixed
    /// when its process is spawned, and this app has no way to swap a live pty's program out from
    /// under a running shell. The next Shell tab picks it up (`Agents::spawn` reads live
    /// settings on every spawn), which is the honest scope - unlike the terminal *font size*,
    /// which really can be applied to a live pane and therefore is
    /// ([`Self::adjust_terminal_font_size`]).
    pub(in crate::settings) fn apply_shell_input(&mut self, cx: &mut Context<Self>) {
        let typed = self.shell_input.as_str().trim();
        self.settings.terminal.shell = (!typed.is_empty()).then(|| typed.to_string());
        self.refresh_shell_status();
        self.persist_settings(cx);
        cx.notify();
    }

    /// Re-probes what the configured shell resolves to right now
    /// ([`crate::settings::state::detect_shell_status`], with the real
    /// `pty_core::resolve_on_path`). Called on every edit and when Settings opens - never from
    /// `render`, which would put a real `$PATH` walk on the frame path.
    ///
    /// A single `$PATH` walk for one name, unlike [`Self::load_agent_rows`]'s walk *per agent
    /// binary*, so this stays on the foreground thread rather than growing a background task and
    /// a stale-result race for a keystroke-frequency operation.
    pub(crate) fn refresh_shell_status(&mut self) {
        self.shell_status = settings::detect_shell_status(
            self.settings.terminal.shell_override(),
            pty_core::resolve_on_path,
        );
    }

    /// The Shell field's suggestion dropdown (GitHub issue #213's follow-up): one clickable row
    /// per shell this machine genuinely has ([`Self::shell_suggestions`]), filtered by whatever is
    /// currently typed, positioned directly under the field.
    ///
    /// **Why it is a top-level sibling in [`AdeApp::render`]** rather than a child of the settings
    /// row: the row lives inside the settings page's own scrolling column, which clips its
    /// children, so a popover nested there would be cut off at the column's edge. The established
    /// answer in this app is the `+` menu's: capture the trigger's window-space bounds with a real
    /// `gpui::canvas` ([`Self::shell_field_bounds`]) and position an `.absolute()` root-level
    /// sibling off them.
    ///
    /// **Chrome is not invented here.** The panel is
    /// [`crate::root::widgets::menu_popover_chrome`] with `theme::shadow::MENU` - the one real
    /// dropdown/context-menu surface every other popover in the app is built from - and the rows
    /// mirror `crate::work_surface::render::render_dropdown_menu_row`'s exact tokens (see
    /// [`Self::render_shell_suggestion_row`] for the one reason they can't literally call it).
    /// The click-away scrim is the file tree context menu's: full-window below the title bar
    /// (never over it - a full-window occluding scrim swallows the caption buttons) and
    /// `.occlude()`d, with the panel calling `cx.stop_propagation()` so a click on a row is not
    /// also a click on the scrim.
    ///
    /// Dismissal is therefore the same as every other menu's, not a new rule: the scrim's click,
    /// opening any other menu surface, the window losing focus
    /// (`crate::root::menus::MenuSurface::ShellSuggestions`), leaving Settings, and picking a row.
    pub(crate) fn render_shell_suggestions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.shell_field_bounds;
        // Right-aligned with the field (which sits at the right edge of its settings row) so the
        // panel, wider than the 168px field, grows inwards over the page rather than off it -
        // the same right-alignment the git graph's row menu uses against its own trigger. Clamped
        // to a small left margin for a genuinely narrow window, mirroring
        // `crate::menu::model::MENU_EDGE_MARGIN`'s own job.
        let left = px(f32::max(
            (bounds.origin.x + bounds.size.width - SHELL_SUGGESTIONS_WIDTH).as_f32(),
            SHELL_SUGGESTIONS_EDGE_MARGIN,
        ));
        // The scrim starts below the title bar, so positions measured in window space have to be
        // rebased into it - exactly what `render_tree_context_menu` does.
        let top = bounds.origin.y + bounds.size.height + px(4.0) - theme::band::TITLE_BAR;
        let matches =
            settings::filter_shell_suggestions(&self.shell_suggestions, self.shell_input.as_str());

        div()
            .id("settings-shell-suggestions-scrim")
            .absolute()
            .top(theme::band::TITLE_BAR)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .occlude()
            .bg(work_surface::TRANSPARENT)
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.shell_suggestions_open = false;
                cx.notify();
            }))
            .child(
                menu_popover_chrome(
                    div()
                        .id("settings-shell-suggestions-popover")
                        .debug_selector(|| "settings-shell-suggestions-popover".to_string())
                        .absolute()
                        .left(left)
                        .top(top)
                        .w(SHELL_SUGGESTIONS_WIDTH)
                        .py(px(4.0)),
                    theme::shadow::MENU,
                )
                .occlude()
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .child(
                    div()
                        .px(px(10.0))
                        .pb(px(3.0))
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(9.0))
                        .text_color(theme::text::GHOST)
                        .child("detected on this machine"),
                )
                .child(
                    div()
                        .id("settings-shell-suggestions-list")
                        .flex()
                        .flex_col()
                        .max_h(SHELL_SUGGESTIONS_MAX_HEIGHT)
                        .overflow_y_scroll()
                        .children(matches.iter().enumerate().map(|(index, suggestion)| {
                            self.render_shell_suggestion_row(index, suggestion, cx)
                        })),
                ),
            )
    }

    /// One suggestion row - a `❯` chip (the same glyph the `+` menu's own "New terminal" row
    /// uses), the shell's real name, and the real absolute path it was found at, so the user can
    /// tell `/bin/bash` from `/usr/local/bin/bash` before clicking. Clicking types that path into
    /// the field ([`Self::select_shell_suggestion`]).
    ///
    /// Visually this is `crate::work_surface::render::render_dropdown_menu_row` - same 29px band,
    /// same 10px padding and 9px gap, same 14×14 chip, same label/sub type ramp, same
    /// `theme::surface::MENU_ROW_HOVER` hover. It cannot literally call that function for one real
    /// reason: that helper keys its GPUI element id off a `&'static str` label, and a shell's name
    /// is neither `'static` nor guaranteed unique (a machine can genuinely have two different
    /// `bash` binaries at two different paths), so the ids would collide. The row's identity is
    /// its position in the filtered list instead.
    fn render_shell_suggestion_row(
        &self,
        index: usize,
        suggestion: &settings::ShellSuggestion,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let value = suggestion.value();
        div()
            .id(("settings-shell-suggestion", index))
            .debug_selector(move || format!("settings-shell-suggestion-{index}"))
            .flex()
            .items_center()
            .gap(px(9.0))
            .h(theme::band::PLUS_MENU_ROW)
            .px(px(10.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme::surface::MENU_ROW_HOVER))
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.select_shell_suggestion(value.clone(), window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::surface::CHIP_NEUTRAL)
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(8.0))
                    .text_color(theme::text::DIM)
                    .child("\u{276f}"),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(11.5))
                    .text_color(theme::text::HEADING)
                    .child(suggestion.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::FAINTER)
                    .child(suggestion.value()),
            )
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
        let bracket_pair_row = self.render_settings_row(
            "Bracket pair colors",
            "Color matching brackets by nesting depth, so a pair and its contents are easy to trace.",
            self.render_toggle_control(
                "settings-bracket-pair-colorization",
                self.settings.appearance.bracket_pair_colorization,
                cx,
                |this, cx| this.toggle_bracket_pair_colorization(cx),
            ),
        );
        let indent_guides_row = self.render_settings_row(
            "Indent guides",
            "Vertical lines marking each level of leading indentation in the code editor.",
            self.render_toggle_control(
                "settings-indent-guides",
                self.settings.appearance.show_indent_guides,
                cx,
                |this, cx| this.toggle_indent_guides(cx),
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
            .child(self.render_display_scale_override_rows(cx))
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
            .child(indent_guides_row)
            .child(bracket_pair_row)
            .child(self.render_snippet_block(settings_store::ConfigPage::Appearance))
    }

    /// GitHub issue #216's rows: a toggle that turns
    /// [`settings_store::AppearanceSettings::display_scale_override`] from `None` (GPUI detects
    /// the scale, as it always has) into a real forced factor, plus - only while it is on - a
    /// stepper for that factor.
    ///
    /// Two rows rather than one control, because the setting is genuinely two states: "leave
    /// detection alone" is not the same as "force 1.0", and a bare stepper could not express the
    /// first. The stepper is hidden rather than disabled while the override is off, so the page
    /// never shows a number that isn't being used.
    ///
    /// Built as a `#[cfg]` pair of same-named methods - the idiom
    /// `crate::status_bar::render::AdeApp::render_status_agents_cluster` already established for a
    /// platform-conditional *rendered* element - scoped to the two targets whose `gpui_platform`
    /// dependency requests the `x11` feature (`crates/app/Cargo.toml`). Everywhere else the
    /// variable this writes is never read by anything, so the row would be a control bound to
    /// nothing.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn render_display_scale_override_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let override_factor = self.settings.appearance.display_scale_override;

        let toggle_row = self.render_settings_row(
            "Override display scale",
            "Ignore the scale GPUI detects for this display. X11 sessions only, not Wayland - \
             takes effect the next time Jerry starts.",
            self.render_toggle_control(
                "settings-display-scale-override",
                override_factor.is_some(),
                cx,
                |this, cx| this.toggle_display_scale_override(cx),
            ),
        );
        let factor_row = override_factor.map(|factor| {
            self.render_settings_row(
                "Forced scale factor",
                "1.00\u{d7} is unscaled - what a display that reports no scaling should look like.",
                self.render_stepper_control(
                    "settings-display-scale-factor",
                    format!("{factor:.2}\u{d7}"),
                    cx,
                    |this, cx| {
                        this.adjust_display_scale_override(
                            -settings_store::DISPLAY_SCALE_OVERRIDE_STEP,
                            cx,
                        )
                    },
                    |this, cx| {
                        this.adjust_display_scale_override(
                            settings_store::DISPLAY_SCALE_OVERRIDE_STEP,
                            cx,
                        )
                    },
                ),
            )
        });

        div()
            .flex()
            .flex_col()
            .child(toggle_row)
            .children(factor_row)
    }

    /// Non-X11 build: this setting's only real effect is `GPUI_X11_SCALE_FACTOR`, which nothing on
    /// this platform reads (see the `#[cfg]` twin above), so the Appearance page shows no row at
    /// all rather than a switch that would persist a value and change nothing.
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    fn render_display_scale_override_rows(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
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
            // GitHub issue #128 - matches `Self::render_theme_card`'s own identical hover, the
            // adjacent card widget this one is otherwise styled just like.
            .when(!is_selected, |el| {
                el.hover(|el| el.border_color(theme::settings::THEME_CARD_HOVER_BORDER))
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

    /// *Themes* - the six cards from `crate::settings::state::THEME_DEFS`, with persisted
    /// selection. Selecting a card persists (`Self::settings.theme.name` round-trips through
    /// `settings.toml`) **and** really re-skins the running app: `crate::theme`'s ~270 colour
    /// tokens are each a `crate::theme::ColorToken`, resolved against the live palette
    /// `Self::apply_theme_selection` compiles from the selected theme's own file (and everything
    /// up its `base` chain) rather than against a plain compile-time constant - see that module's
    /// own docs for the runtime mechanism. `Self::set_theme_name` is the one real place a
    /// selection is applied: it installs the compiled palette and forces a real full repaint
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
                    self.render_theme_card(
                        def.name,
                        def.subtitle,
                        def.theme.preview_swatches(),
                        false,
                        cx,
                    )
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
            .child(self.render_icon_pack_section(cx))
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
                        "settings-theme-import-vscode",
                        "Import VSCode theme\u{2026}",
                        cx,
                        |this, cx| this.start_import_vscode_theme(cx),
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
                self.render_theme_card(
                    &theme.name,
                    &theme.subtitle,
                    theme.preview_swatches(),
                    true,
                    cx,
                )
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
            .child(self.render_theme_seed_row(cx))
            .children(load_errors)
            .children(status)
    }

    /// GitHub issue #141's "Generate from colour" row: a real, focusable `#rrggbb` input plus the
    /// button that turns it into a whole theme file.
    ///
    /// This is where `crate::theme::derive_shift`'s HSL machinery ended up after the theme
    /// system's rewrite took it out of the live resolution path: one seed colour becomes a real,
    /// complete, literal, hand-editable theme file on disk (`Self::generate_theme_from_seed`),
    /// derived by exactly the same code that generated the five bundled themes' own files
    /// (`custom_theme`/`builtin_themes`). It is a starting point, not a black box - the whole
    /// point of writing it out as ~270 explicit lines is that the user can then retune any one of
    /// them by hand.
    ///
    /// The input is the same minimal hand-rolled field shape as the Keybindings filter
    /// ([`Self::render_settings_keymap_filter_row`]) - a real `FocusHandle`, a real caret
    /// ([`Self::render_simple_input_caret`]), append/backspace/`Esc`-clears - deliberately reusing
    /// that established pattern rather than introducing a second, richer text-input mechanism this
    /// app doesn't otherwise have.
    fn render_theme_seed_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let seed = self.theme_seed_input.as_str().to_string();
        let has_seed = !seed.is_empty();

        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pt(px(10.0))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(px(10.5))
                    .text_color(theme::text::MUTED)
                    .child("Generate from colour"),
            )
            .child(
                self.wire_text_input_actions(
                    div()
                        .id("settings-theme-seed-input")
                        .debug_selector(|| "settings-theme-seed-input".to_string())
                        .track_focus(&self.theme_seed_focus_handle)
                        // See `crate::default_key_bindings`' `TextUndo`/`TextRedo` docs for why
                        // the tag and the listeners both live on this exact node.
                        .key_context("text-input")
                        .on_action(cx.listener(Self::handle_theme_seed_text_undo))
                        .on_action(cx.listener(Self::handle_theme_seed_text_redo))
                        .on_key_down(cx.listener(Self::handle_theme_seed_key_down)),
                    theme_seed_input_handle(),
                    cx,
                )
                    .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        window.focus(&this.theme_seed_focus_handle, cx);
                    }))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    // No decorative gap before the caret - see
                    // `crate::rail::render::AdeApp::render_rail_filter_row`'s own
                    // comment for why (live report: it read as a gap between the
                    // typed text and where it's actually being typed).
                    .h(px(20.0))
                    .w(px(96.0))
                    .px(px(7.0))
                    .rounded(theme::radius::BUTTON)
                    .border_1()
                    .border_color(theme::border::CARD_FIELD)
                    .bg(theme::surface::CARD_SUNK)
                    // GitHub issue #336: through the one helper that owns this structure, like
                    // every other simple input in the app, rather than the hand-assembled
                    // caret-before-placeholder / text / caret-after-text trio this row used to
                    // carry - which is exactly the duplication `render_simple_input_row` exists to
                    // end, and which had no selection highlight or hit-testing of its own.
                    .child(self.render_simple_input_row(
                        SimpleInput {
                            caret_selector: "settings-theme-seed-caret".into(),
                            text_selector: "settings-theme-seed-text".into(),
                            focus_handle: Some(&self.theme_seed_focus_handle),
                            text: if has_seed { seed.as_str() } else { "" },
                            caret_offset: self.theme_seed_input.caret(),
                            selection: self.theme_seed_input.selection(),
                            placeholder: "#rrggbb",
                            font: theme::font::MONO,
                            text_size: px(10.5),
                            text_color: theme::text::BODY,
                            placeholder_color: theme::text::GHOST,
                            field: Some(theme_seed_input_handle()),
                        },
                        cx,
                    )),
            )
            // A real, live preview of the seed itself, so a typo is visible before clicking.
            .when(parse_seed_hex(&seed).is_some(), |el| {
                let value = parse_seed_hex(&seed).unwrap_or(0);
                el.child(
                    div()
                        .w(px(20.0))
                        .h(px(14.0))
                        .rounded(theme::radius::MARK)
                        .border_1()
                        .border_color(theme::border::CARD)
                        .bg(gpui::rgb(value)),
                )
            })
            .child(self.render_theme_action_button(
                "settings-theme-generate",
                "Generate\u{2026}",
                cx,
                |this, cx| this.start_generate_theme_from_seed(cx),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::DISABLED)
                    .child(
                        "one colour becomes a whole editable theme file, tuned around it"
                            .to_string(),
                    ),
            )
    }

    /// Same minimal append/backspace/escape-clears shape as
    /// [`Self::handle_settings_keymap_filter_key_down`] - see that method's docs for the
    /// deliberate scope cut (no cursor positioning, no selection, no IME).
    pub(in crate::settings) fn handle_theme_seed_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // GitHub issue #336: `widgets::text_editing_modifiers` rather than a flat "any modifier
        // means not ours" - see `crate::rail::render::AdeApp::handle_filter_key_down`'s own note.
        let Some(modifiers) = widgets::text_editing_modifiers(&keystroke.key, &keystroke.modifiers)
        else {
            return;
        };
        self.reset_caret_blink(cx);
        let changed = match keystroke.key.as_str() {
            "escape" => self.theme_seed_input.clear(Instant::now()),
            "enter" => {
                self.start_generate_theme_from_seed(cx);
                return;
            }
            key => self.theme_seed_input.handle_editing_key(
                key,
                keystroke.key_char.as_deref(),
                modifiers,
                Instant::now(),
            ),
        };
        if changed {
            cx.notify();
        }
    }

    /// GitHub issue #141's real "Generate from colour" action: parses the seed, derives a whole
    /// palette from it (`crate::theme::shift_from_seed` + `derived_palette` - the exact same
    /// generator the five bundled theme files were produced with), and writes it into this
    /// instance's own themes directory through the same validate-then-write-then-reload-from-disk
    /// path every other theme-creating action uses, on the background executor.
    ///
    /// A malformed or empty seed is a real, specific status-line error rather than a silent no-op
    /// or a guessed default - there is no honest colour to fall back to when the whole action is
    /// "build a theme around *this* colour".
    pub(in crate::settings) fn start_generate_theme_from_seed(&mut self, cx: &mut Context<Self>) {
        let Some(seed) = parse_seed_hex(self.theme_seed_input.as_str()) else {
            self.custom_theme_status = Some(Err(format!(
                "\"{}\" isn't a colour - type a hex value like #7f9ad4 to generate a theme from",
                self.theme_seed_input.as_str()
            )));
            cx.notify();
            return;
        };
        let Some(settings_path) = self.settings_path.clone() else {
            self.custom_theme_status = Some(Err(
                "can't create a theme: no settings file location is known".to_string(),
            ));
            cx.notify();
            return;
        };
        let file = generated_theme_file_for_seed(seed);
        let task = cx.spawn(async move |this, cx| {
            let dest_dir = custom_theme::custom_themes_dir_for(&settings_path);
            let result: Result<_, custom_theme::ThemeFileError> = cx
                .background_executor()
                .spawn(async move {
                    let created = custom_theme::validate_and_write(file, &dest_dir)?;
                    let (themes, errors) = custom_theme::load_custom_themes_from_dir(&dest_dir);
                    Ok((created, themes, errors))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_custom_theme_load_result(
                    result,
                    |name| format!("Generated \"{name}\" - edit its file to tune it further."),
                    cx,
                );
            });
        });
        self._theme_generate_task = Some(task);
    }

    /// GitHub issue #5's "custom icon packs": a real, user-chosen directory of `<name>.svg`
    /// files (`crate::icon_pack::resolve_icon`'s own lookup) that overrides this app's own
    /// default icons wherever a call site has been wired to check one - today, only the rail's
    /// agent-kind chip (`AdeApp::render_agent_chip_icon`'s own docs list the real, current
    /// scope). No active pack, or a pack missing a given name, always falls back to this app's
    /// existing default look - never a broken icon or a blank chip.
    fn render_icon_pack_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self
            .settings
            .icon_pack
            .directory
            .as_ref()
            .map(|path| path.display().to_string());

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
                    .child("Icon pack"),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(self.render_theme_action_button(
                        "settings-icon-pack-choose",
                        "Choose folder\u{2026}",
                        cx,
                        |this, cx| this.start_choose_icon_pack_folder(cx),
                    ))
                    .when(current.is_some(), |el| {
                        el.child(self.render_theme_action_button(
                            "settings-icon-pack-clear",
                            "Use default icons",
                            cx,
                            |this, cx| this.clear_icon_pack(cx),
                        ))
                    }),
            );

        let current_row = div()
            .py(px(6.0))
            .font(font(theme::font::MONO))
            .text_size(px(10.5))
            .text_color(theme::text::DISABLED)
            .child(match &current {
                Some(path) => format!(
                    "Using {path} - files named e.g. claude.svg/codex.svg/shell.svg override \
                     the matching default icon."
                ),
                None => "Using the app's default icons. Choose a folder containing files like \
                          claude.svg/codex.svg/shell.svg to override them."
                    .to_string(),
            });

        let status = self.icon_pack_status.as_ref().map(|result| {
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
            .child(current_row)
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
        self.wire_text_input_actions(
            div()
                .id("settings-keymap-filter")
                .track_focus(&self.settings_keymap_filter_focus_handle)
                // See `crate::default_key_bindings`' `TextUndo`/`TextRedo` docs for why the tag
                // and the listener both live on this exact node.
                .key_context("text-input")
                .on_action(cx.listener(Self::handle_settings_keymap_filter_text_undo))
                .on_action(cx.listener(Self::handle_settings_keymap_filter_text_redo))
                .on_key_down(cx.listener(Self::handle_settings_keymap_filter_key_down)),
            settings_keymap_filter_handle(),
            cx,
        )
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
                    // No decorative gap before the caret - see
                    // `crate::rail::render::AdeApp::render_rail_filter_row`'s own
                    // comment for why (live report: it read as a gap between the
                    // typed text and where it's actually being typed).
                    // GitHub issue #45 / live report: same fix as
                    // `crate::rail::render::AdeApp::render_rail_filter_row` - the caret sits
                    // before the placeholder (real cursor position 0) while the filter is empty,
                    // never appended after it.
                    .child(self.render_simple_input_row(
                        SimpleInput {
                            caret_selector: "settings-keymap-filter-caret".into(),
                            text_selector: "settings-keymap-filter-text".into(),
                            focus_handle: Some(&self.settings_keymap_filter_focus_handle),
                            text: self.settings_keymap_filter.as_str(),
                            caret_offset: self.settings_keymap_filter.caret(),
                            selection: self.settings_keymap_filter.selection(),
                            placeholder: &format!(
                                "filter {}",
                                plural::count(total, "binding", None)
                            ),
                            font: theme::font::SANS,
                            text_size: px(11.0),
                            text_color: theme::text::DIM,
                            placeholder_color: theme::text::GHOST,
                            field: Some(settings_keymap_filter_handle()),
                        },
                        cx,
                    )),
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
        // GitHub issue #336: `widgets::text_editing_modifiers` rather than a flat "any modifier
        // means not ours" - see `crate::rail::render::AdeApp::handle_filter_key_down`'s own note.
        let Some(modifiers) = widgets::text_editing_modifiers(&keystroke.key, &keystroke.modifiers)
        else {
            return;
        };
        // GitHub issue #27's "solid mid-keystroke" - see `crate::palette::render::AdeApp::
        // handle_palette_key_down`'s identical reasoning.
        self.reset_caret_blink(cx);
        let changed = match keystroke.key.as_str() {
            // A real, undoable step - see `crate::rail::AdeApp::handle_filter_key_down`'s own
            // identical `Esc` handling.
            "escape" => self.settings_keymap_filter.clear(Instant::now()),
            key => self.settings_keymap_filter.handle_editing_key(
                key,
                keystroke.key_char.as_deref(),
                modifiers,
                Instant::now(),
            ),
        };
        if changed {
            cx.notify();
            cx.stop_propagation();
        }
    }

    /// `TextUndo`/`TextRedo` for the Keybindings page's filter field (GitHub issue #17) - see
    /// `crate::default_key_bindings`' own docs for the scoping.
    pub(in crate::settings) fn handle_theme_seed_text_undo(
        &mut self,
        _: &TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.theme_seed_input.undo() {
            cx.notify();
        }
    }

    pub(in crate::settings) fn handle_theme_seed_text_redo(
        &mut self,
        _: &TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.theme_seed_input.redo() {
            cx.notify();
        }
    }

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
        let suggest_auto_imports_row = self.render_settings_row(
            "Suggest auto-imports",
            "Offer completions for symbols this file hasn't imported yet. Turn this off in a \
             browser project, where an installed @types/node drags Node's whole API into every \
             list. Currently applies to TypeScript and JavaScript only - the equivalent for other \
             servers isn't wired yet, and isn't faked.",
            self.render_toggle_control(
                "settings-editor-suggest-auto-imports",
                self.settings.editor.suggest_auto_imports,
                cx,
                |this, cx| this.toggle_suggest_auto_imports(cx),
            ),
        );
        let auto_import_row = self.render_settings_row(
            "Auto-import on accept",
            "When a completion comes from a module this file doesn't import yet, accepting it \
             also writes the import line the language server asks for. Turn this off to insert \
             just the name - useful in a browser project, where a server will happily offer \
             Node's own modules that the bundler then can't resolve.",
            self.render_toggle_control(
                "settings-editor-auto-import",
                self.settings.editor.auto_import,
                cx,
                |this, cx| this.toggle_auto_import(cx),
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
            .child(
                div()
                    .pt(px(20.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Completions"),
            )
            .child(suggest_auto_imports_row)
            .child(auto_import_row)
            .child(self.render_snippet_block(settings_store::ConfigPage::Editor))
    }

    /// *Notifications* (GitHub issue #226): the sound design module. Master switch, one row per
    /// [`crate::sound::SoundEventKind`] (its own toggle + a dropdown choosing which library sound
    /// it plays), then the sound library itself (built-in and imported sounds, each with a real
    /// ▶ preview) with the same import/open-folder actions the Themes page's custom-theme section
    /// already established (`Self::start_import_custom_theme`/`Self::open_custom_themes_folder`).
    pub(in crate::settings) fn render_settings_notifications_page(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sound = &self.settings.sound;
        let master_row = self.render_settings_row(
            "Sound effects",
            "The master switch for every sound below. Off by default - turning this on enables \
             all three events below at once; disable the ones you don't want individually.",
            self.render_toggle_control("settings-sound-enabled", sound.enabled, cx, |this, cx| {
                this.toggle_sound_enabled(cx)
            }),
        );

        div()
            .flex()
            .flex_col()
            .child(self.render_config_banner(settings_store::ConfigPage::Notifications, cx))
            .child(
                div()
                    .pt(px(20.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Sounds"),
            )
            .child(master_row)
            .children(SoundEventKind::ALL.map(|event| self.render_sound_event_row(event, cx)))
            .child(
                div()
                    .pt(px(20.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Sound library"),
            )
            .child(self.render_sound_library_section(cx))
            .child(self.render_snippet_block(settings_store::ConfigPage::Notifications))
    }

    /// One [`SoundEventKind`] row: label/hint from the event itself, a "choose a sound" trigger
    /// button (opens [`Self::render_sound_picker`], a floating popover - see that method's own
    /// docs for why it can't just be a child of this row), and the event's own on/off toggle.
    ///
    /// While the master "Sound effects" switch is off, every event row is genuinely inert - none
    /// of its own settings has any effect until the master is back on - so both the trigger and
    /// the toggle are rendered non-interactive (`interactive: false`) and the whole row is dimmed
    /// to [`SOUND_ROW_DISABLED_OPACITY`], rather than leaving three controls on screen that look
    /// live but silently do nothing when clicked.
    fn render_sound_event_row(
        &self,
        event: SoundEventKind,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let event_settings = self.sound_event_settings(event);
        let current_name =
            crate::sound::library::resolve(&event_settings.sound, &self.sound_library)
                .map(|sound| sound.name.clone())
                .unwrap_or_else(|| "Choose a sound…".to_string());
        let interactive = self.settings.sound.enabled;

        let control = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(self.render_sound_picker_trigger(event, &current_name, interactive, cx))
            .child(self.render_toggle_control_gated(
                match event {
                    SoundEventKind::AppStart => "settings-sound-app-start-enabled",
                    SoundEventKind::AgentFinished => "settings-sound-agent-finished-enabled",
                    SoundEventKind::AgentNeedsInput => "settings-sound-agent-needs-input-enabled",
                },
                event_settings.enabled,
                interactive,
                cx,
                move |this, cx| this.toggle_sound_event_enabled(event, cx),
            ));

        div()
            .when(!interactive, |el| el.opacity(SOUND_ROW_DISABLED_OPACITY))
            .child(self.render_settings_row(event.label(), event.description(), control))
    }

    /// The event row's own clickable "which sound" field - deliberately shaped like
    /// [`Self::render_settings_shell_control`]'s field rather than
    /// [`Self::render_choice_control`]'s segmented control: a segmented control reads fine for
    /// three options and unreadable for a library that can grow past a handful of imports (see
    /// GitHub issue #226's own scoping decision), where a dropdown scales.
    ///
    /// `interactive` mirrors `Self::render_toggle_control_gated`'s own flag - `false` while the
    /// master "Sound effects" switch is off, so this field can't open its popover for a setting
    /// that currently has no effect. The bounds-capturing canvas below stays regardless: it never
    /// opens anything on its own, and dropping it would leave `Self::sound_event_button_bounds`
    /// stale the next time the row does become interactive.
    fn render_sound_picker_trigger(
        &self,
        event: SoundEventKind,
        current_name: &str,
        interactive: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current_name = current_name.to_string();
        div()
            .id(format!(
                "settings-sound-picker-trigger-{}",
                event.settings_key()
            ))
            .debug_selector(move || {
                format!("settings-sound-picker-trigger-{}", event.settings_key())
            })
            .when(interactive, |el| {
                el.cursor_pointer().on_click(cx.listener(
                    move |this, _event: &ClickEvent, _window, cx| {
                        this.open_sound_picker(event, cx);
                    },
                ))
            })
            // The trigger's real, window-space painted bounds, for positioning its popover - same
            // `gpui::canvas` idiom `Self::shell_field_bounds` uses, kept per-event in
            // `Self::sound_event_button_bounds` since all three rows are on screen at once.
            .child({
                let this = cx.entity();
                gpui::canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, _cx| {
                            this.sound_event_button_bounds.insert(event, bounds);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(20.0))
            .px(px(7.0))
            .w(px(168.0))
            .rounded(theme::radius::BUTTON)
            .border_1()
            .border_color(theme::border::CARD_FIELD)
            .bg(theme::surface::CARD_SUNK)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::BODY)
                    .child(current_name),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(8.0))
                    .text_color(theme::text::FAINTER)
                    .child("\u{25be}"),
            )
    }

    /// The sound library section: one row per [`crate::sound::LibrarySound`] (name, a
    /// Built-in/Imported badge, a ▶ preview), the real load-error list
    /// ([`Self::sound_load_errors`]) if any, the most recent import status
    /// ([`Self::sound_import_status`]), and the two real actions - same shape as the Themes
    /// page's custom-theme section.
    fn render_sound_library_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .child(
                div().flex().flex_col().children(
                    self.sound_library
                        .iter()
                        .enumerate()
                        .map(|(index, sound)| self.render_sound_library_row(index, sound, cx)),
                ),
            )
            .when(!self.sound_load_errors.is_empty(), |el| {
                el.child(div().mt(px(8.0)).flex().flex_col().gap(px(2.0)).children(
                    self.sound_load_errors.iter().map(|error| {
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(10.0))
                            .text_color(theme::status::FAIL)
                            .child(error.clone())
                    }),
                ))
            })
            .when_some(self.sound_import_status.as_ref(), |el, status| {
                let (text, color) = match status {
                    Ok(message) => (message.clone(), theme::status::REVIEW),
                    Err(message) => (message.clone(), theme::status::FAIL),
                };
                el.child(
                    div()
                        .mt(px(8.0))
                        .font(font(theme::font::SANS))
                        .text_size(self.ui_text_size(10.5))
                        .text_color(color)
                        .child(text),
                )
            })
            .child(
                div()
                    .mt(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("settings-sound-import")
                            .debug_selector(|| "settings-sound-import".to_string())
                            .cursor_pointer()
                            .h(px(22.0))
                            .px(px(10.0))
                            .rounded(theme::radius::BUTTON)
                            .border_1()
                            .border_color(theme::border::BUTTON)
                            .flex()
                            .items_center()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(10.5))
                            .text_color(theme::text::MUTED)
                            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                            .child("Import sound\u{2026}")
                            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.start_import_sound(cx);
                            })),
                    )
                    .child(
                        div()
                            .id("settings-sound-open-folder")
                            .debug_selector(|| "settings-sound-open-folder".to_string())
                            .cursor_pointer()
                            .h(px(22.0))
                            .px(px(10.0))
                            .rounded(theme::radius::BUTTON)
                            .border_1()
                            .border_color(theme::border::BUTTON)
                            .flex()
                            .items_center()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(10.5))
                            .text_color(theme::text::MUTED)
                            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                            .child("Open sounds folder")
                            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.open_sounds_folder(cx);
                            })),
                    ),
            )
    }

    fn render_sound_library_row(
        &self,
        index: usize,
        sound: &crate::sound::LibrarySound,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sound_id = sound.id.clone();
        div()
            .id(("settings-sound-library-row", index))
            .flex()
            .items_center()
            .gap(px(9.0))
            .py(px(7.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .id(("settings-sound-preview", index))
                    .debug_selector(move || format!("settings-sound-preview-{index}"))
                    .cursor_pointer()
                    .flex_none()
                    .w(px(20.0))
                    .h(px(20.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::surface::CHIP_NEUTRAL)
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .font(font(theme::font::SANS))
                    .text_size(px(8.0))
                    .text_color(theme::text::DIM)
                    .child("\u{25b6}")
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, _cx| {
                        this.preview_sound(&sound_id, None);
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(11.5))
                    .text_color(theme::text::BODY)
                    .child(sound.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(6.0))
                    .h(px(16.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .bg(theme::surface::CHIP_NEUTRAL)
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(8.5))
                    .text_color(theme::text::DIM)
                    .child(if sound.is_builtin() {
                        "Built-in"
                    } else {
                        "Imported"
                    }),
            )
    }

    /// The event picker's own floating popover - same reasoning as
    /// [`Self::render_shell_suggestions`]'s own docs ("why it is a top-level sibling"): the
    /// settings page is a scrolling column that clips its children, so this can only paint in
    /// full as a root-level sibling positioned off the clicked trigger's own
    /// [`Self::sound_event_button_bounds`] entry. Only ever called while
    /// [`Self::sound_picker_open`] is `Some`.
    pub(crate) fn render_sound_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let event = self.sound_picker_open.unwrap_or(SoundEventKind::AppStart);
        let bounds = self
            .sound_event_button_bounds
            .get(&event)
            .copied()
            .unwrap_or_default();
        let left = px(f32::max(
            (bounds.origin.x + bounds.size.width - SOUND_PICKER_WIDTH).as_f32(),
            SOUND_PICKER_EDGE_MARGIN,
        ));
        let top = bounds.origin.y + bounds.size.height + px(4.0) - theme::band::TITLE_BAR;

        div()
            .id("settings-sound-picker-scrim")
            .absolute()
            .top(theme::band::TITLE_BAR)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .occlude()
            .bg(work_surface::TRANSPARENT)
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.sound_picker_open = None;
                cx.notify();
            }))
            .child(
                menu_popover_chrome(
                    div()
                        .id("settings-sound-picker-popover")
                        .debug_selector(|| "settings-sound-picker-popover".to_string())
                        .absolute()
                        .left(left)
                        .top(top)
                        .w(SOUND_PICKER_WIDTH)
                        .py(px(4.0))
                        .max_h(SOUND_PICKER_MAX_HEIGHT)
                        .overflow_y_scroll(),
                    theme::shadow::MENU,
                )
                .occlude()
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .children(
                    self.sound_library.iter().enumerate().map(|(index, sound)| {
                        self.render_sound_picker_row(index, sound, event, cx)
                    }),
                ),
            )
    }

    fn render_sound_picker_row(
        &self,
        index: usize,
        sound: &crate::sound::LibrarySound,
        event: SoundEventKind,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sound_id = sound.id.clone();
        let selected = self.sound_event_settings(event).sound == sound.id;
        div()
            .id(("settings-sound-picker-row", index))
            .debug_selector(move || format!("settings-sound-picker-row-{index}"))
            .flex()
            .items_center()
            .gap(px(9.0))
            .h(theme::band::PLUS_MENU_ROW)
            .px(px(10.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme::surface::MENU_ROW_HOVER))
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.select_sound_for_event(event, sound_id.clone(), window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(14.0))
                    .font(font(theme::font::MONO))
                    .text_size(px(9.0))
                    .text_color(theme::text::SELECTED)
                    .child(if selected { "\u{2713}" } else { "" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::BODY)
                    .child(sound.name.clone()),
            )
    }

    /// The live [`settings_store::SoundEventSettings`] for `event` - the one place this file reads
    /// which of the three `Settings.sound.*` fields an event maps to, so every row/setter above
    /// stays a plain `match` on [`SoundEventKind`] rather than three near-duplicate call sites.
    fn sound_event_settings(&self, event: SoundEventKind) -> &settings_store::SoundEventSettings {
        match event {
            SoundEventKind::AppStart => &self.settings.sound.app_start,
            SoundEventKind::AgentFinished => &self.settings.sound.agent_finished,
            SoundEventKind::AgentNeedsInput => &self.settings.sound.agent_needs_input,
        }
    }

    fn sound_event_settings_mut(
        &mut self,
        event: SoundEventKind,
    ) -> &mut settings_store::SoundEventSettings {
        match event {
            SoundEventKind::AppStart => &mut self.settings.sound.app_start,
            SoundEventKind::AgentFinished => &mut self.settings.sound.agent_finished,
            SoundEventKind::AgentNeedsInput => &mut self.settings.sound.agent_needs_input,
        }
    }

    fn toggle_sound_enabled(&mut self, cx: &mut Context<Self>) {
        self.settings.sound.enabled = !self.settings.sound.enabled;
        // Turning the master off makes every event row non-interactive - an open sound picker
        // popover would otherwise survive pointing at a trigger that no longer responds to
        // clicks.
        if !self.settings.sound.enabled {
            self.sound_picker_open = None;
        }
        self.persist_settings(cx);
        cx.notify();
    }

    /// Flips one event's own toggle. Switching it *on* plays a preview of whatever sound that
    /// event is currently configured with - the same "hearing what you just enabled" feedback
    /// [`Self::select_sound_for_event`] gives when the sound itself changes. Switching it off
    /// plays nothing.
    fn toggle_sound_event_enabled(&mut self, event: SoundEventKind, cx: &mut Context<Self>) {
        let now_enabled = !self.sound_event_settings(event).enabled;
        self.sound_event_settings_mut(event).enabled = now_enabled;
        self.persist_settings(cx);
        if now_enabled {
            let sound_id = self.sound_event_settings(event).sound.clone();
            self.preview_sound(&sound_id, Some(event));
        }
        cx.notify();
    }

    /// Opens `event`'s "choose a sound" popover, closing any other open menu surface first
    /// (GitHub issue #176's shared invariant - see `crate::root::menus::MenuSurface::SoundPicker`).
    pub(in crate::settings) fn open_sound_picker(
        &mut self,
        event: SoundEventKind,
        cx: &mut Context<Self>,
    ) {
        let _ = self.close_menu_surfaces_except(Some(menus::MenuSurface::SoundPicker));
        self.sound_picker_open = Some(event);
        cx.notify();
    }

    /// Assigns `sound_id` to `event`, persists it, closes the popover, and plays a preview of the
    /// newly chosen sound - the explicit-user-action feedback GitHub issue #226's spec calls for
    /// ("choosing a sound plays it"), regardless of whether the event's own toggle or the master
    /// switch happen to be on right now (`Self::preview_sound` is deliberately ungated - see its
    /// own docs).
    pub(in crate::settings) fn select_sound_for_event(
        &mut self,
        event: SoundEventKind,
        sound_id: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sound_event_settings_mut(event).sound = sound_id.clone();
        self.persist_settings(cx);
        self.sound_picker_open = None;
        self.preview_sound(&sound_id, Some(event));
        cx.notify();
    }

    /// "Open sounds folder" - hands the real sounds directory
    /// ([`crate::sound::library::sounds_dir_for`]) to [`Self::open_path_with_os_handler`], same
    /// real per-platform default-open handler as
    /// [`Self::start_open_custom_themes_folder`] (this method's own template). Creates the
    /// directory first if it doesn't exist yet, on the background executor - a user who has never
    /// imported a sound has no real directory there otherwise.
    pub(in crate::settings) fn open_sounds_folder(&mut self, cx: &mut Context<Self>) {
        let Some(settings_path) = self.settings_path.clone() else {
            self.sound_import_status = Some(Err(
                "can't open the sounds folder: no settings file location is known".to_string(),
            ));
            cx.notify();
            return;
        };
        let dest_dir = crate::sound::library::sounds_dir_for(&settings_path);
        cx.spawn(async move |this, cx| {
            let mkdir_dir = dest_dir.clone();
            let mkdir_result = cx
                .background_executor()
                .spawn(async move { std::fs::create_dir_all(&mkdir_dir) })
                .await;
            if let Err(err) = mkdir_result {
                log::warn!(
                    "failed to create the sounds directory {}: {err}",
                    dest_dir.display()
                );
            }
            let _ = this.update(cx, |this, cx| {
                this.open_path_with_os_handler(&dest_dir, cx);
            });
        })
        .detach();
    }

    /// "Import sound…" - a genuine native file-open dialog (`gpui::App::prompt_for_paths`), same
    /// real API [`Self::start_import_custom_theme`] uses. The user picks any real
    /// `.wav`/`.mp3`/`.ogg` file on disk; the actual validate-decode-and-copy runs on the
    /// background executor (`crate::sound::library::import_sound_file`), and the library is then
    /// **reloaded from disk** rather than the newly imported sound spliced into
    /// [`Self::sound_library`] in memory - the same discipline
    /// [`Self::start_import_custom_theme`]'s own docs explain (an id-collision resolution that
    /// picked a different final filename than the naive guess must never leave the in-memory list
    /// disagreeing with what is really on disk). A cancelled dialog is a real, silent no-op.
    pub(in crate::settings) fn start_import_sound(&mut self, cx: &mut Context<Self>) {
        let paths_receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
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
                    this.sound_import_status = Some(Err(
                        "can't import a sound: no settings file location is known".to_string(),
                    ));
                    cx.notify();
                });
                return;
            };
            let dest_dir = crate::sound::library::sounds_dir_for(&settings_path);
            let result = cx
                .background_executor()
                .spawn(async move {
                    let imported =
                        crate::sound::library::import_sound_file(&source_path, &dest_dir)?;
                    let (user_sounds, errors) =
                        crate::sound::library::load_user_sounds_from_dir(&dest_dir);
                    Ok::<_, crate::sound::SoundFileError>((imported, user_sounds, errors))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((imported, user_sounds, errors)) => {
                        let mut library = crate::sound::library::builtin_sounds();
                        library.extend(user_sounds);
                        this.sound_library = library;
                        this.sound_load_errors = errors;
                        this.sound_import_status =
                            Some(Ok(format!("Imported \"{}\".", imported.name)));
                    }
                    Err(err) => {
                        this.sound_import_status = Some(Err(err.to_string()));
                    }
                }
                cx.notify();
            });
        });
        self._sound_import_task = Some(task);
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

    /// GitHub issue #122's real indent-guide toggle - `pub(crate)` like [`Self::toggle_caret_blink`]
    /// above, for the same reason: `indent_guide_tests` (`crate::code_surface::editing`) drives
    /// this directly for its own real-render coverage of whether a guide actually paints.
    /// GitHub issue #168's bracket-pair colorization toggle. Unlike its neighbours this cannot
    /// simply flip-persist-notify: the depth ring is produced during *highlighting*, not at paint
    /// time, so every cached `RenderedLine` computed under the old setting has to be rebuilt -
    /// see [`AdeApp::invalidate_syntax_highlighting`] for the four caches that means and why
    /// nulling them alone isn't enough.
    pub(crate) fn toggle_bracket_pair_colorization(&mut self, cx: &mut Context<Self>) {
        self.settings.appearance.bracket_pair_colorization =
            !self.settings.appearance.bracket_pair_colorization;
        self.persist_settings(cx);
        self.invalidate_syntax_highlighting(cx);
        cx.notify();
    }

    pub(crate) fn toggle_indent_guides(&mut self, cx: &mut Context<Self>) {
        self.settings.appearance.show_indent_guides = !self.settings.appearance.show_indent_guides;
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

    /// GitHub issue #216's toggle: flips
    /// [`settings_store::AppearanceSettings::display_scale_override`] between `None` (GPUI's own
    /// detection) and a real forced factor, starting at
    /// [`settings_store::DISPLAY_SCALE_OVERRIDE_DEFAULT`] - `1.0`, the unscaled value the reported
    /// bug is asking for, so turning the switch on is already the fix in the common case.
    ///
    /// Persist-and-notify only, with no live application, and that is the whole honest story:
    /// GPUI reads `GPUI_X11_SCALE_FACTOR` once while its X11 client initialises, so the new value
    /// is picked up by `crate::main` at the *next* launch. The row's hint says so.
    ///
    /// `#[cfg]`-scoped to match [`Self::render_display_scale_override_rows`], its only caller -
    /// an ungated mutator would be dead code on every other platform.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(in crate::settings) fn toggle_display_scale_override(&mut self, cx: &mut Context<Self>) {
        self.settings.appearance.display_scale_override =
            match self.settings.appearance.display_scale_override {
                Some(_) => None,
                None => Some(settings_store::DISPLAY_SCALE_OVERRIDE_DEFAULT),
            };
        self.persist_settings(cx);
        cx.notify();
    }

    /// GitHub issue #216's stepper. A no-op while the override is off - there is no factor to
    /// step, and inventing one would silently turn the override on from a button the page isn't
    /// even showing then.
    ///
    /// Each result is snapped back to the two decimal places the row displays, rather than left as
    /// whatever repeatedly adding [`settings_store::DISPLAY_SCALE_OVERRIDE_STEP`] to an `f32`
    /// accumulates. Twenty clicks then land on a real `2.00`, not on a `1.9999998` that would look
    /// like `2.00` in the row while being written to `settings.toml` - and exported to GPUI -
    /// verbatim. Two decimals is exactly the step's own precision (`0.05`), so no reachable value
    /// is lost to the snap.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(in crate::settings) fn adjust_display_scale_override(
        &mut self,
        delta: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.settings.appearance.display_scale_override else {
            return;
        };
        let stepped = ((current + delta) * 100.0).round() / 100.0;
        self.settings.appearance.display_scale_override =
            Some(settings_store::sanitize_display_scale_override(stepped));
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

    /// The Editor page's "suggest auto-imports" toggle. Applied at spawn time, so it takes effect
    /// for servers started after it changes - see `crate::lsp::client::AdeApp::ensure_lsp_client`
    /// and `crate::language::auto_import_suppression_options`.
    fn toggle_suggest_auto_imports(&mut self, cx: &mut Context<Self>) {
        self.settings.editor.suggest_auto_imports = !self.settings.editor.suggest_auto_imports;
        self.persist_settings(cx);
        cx.notify();
    }

    /// The Editor page's auto-import toggle -
    /// `crate::lsp::completion_popup::AdeApp::accept_active_completion` reads this on every real
    /// accept. See `settings_store::EditorSettings::auto_import`.
    fn toggle_auto_import(&mut self, cx: &mut Context<Self>) {
        self.settings.editor.auto_import = !self.settings.editor.auto_import;
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
            .theme_window_background_for(&name)
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

    /// Real, single by-name lookup over every theme this app knows about - the six built-in
    /// `settings::THEME_DEFS` first, then [`Self::custom_themes`] - so every caller that has to
    /// resolve a theme name (which palette is live, is it light, what does its card preview
    /// paint, what should exporting it write) can never resolve one differently from another.
    fn theme_by_name(&self, name: &str) -> Option<&custom_theme::CustomTheme> {
        settings::THEME_DEFS
            .iter()
            .find(|def| def.name == name)
            .map(|def| def.theme)
            .or_else(|| self.custom_themes.iter().find(|theme| theme.name == name))
    }

    /// The real window background `name` renders with - [`Self::set_theme_name`]'s
    /// "is this a light theme, for `Settings.theme.last_dark_theme` bookkeeping" question. See
    /// `custom_theme::CustomTheme::window_background` for the one honest approximation involved
    /// (a partial theme inheriting that key from a non-Jerry-Dark base).
    fn theme_window_background_for(&self, name: &str) -> Option<gpui::Rgba> {
        self.theme_by_name(name)
            .map(|theme| theme.window_background())
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
    /// The selection is *compiled* here, once - `custom_theme::compile_palette_by_name` resolves
    /// the name against the six built-in `settings::THEME_DEFS` first, then
    /// [`Self::custom_themes`], walks its whole `base` chain and flattens it into one real
    /// `crate::theme::Palette` that live token resolution then reads with a single hash lookup per
    /// colour. A name matching nothing (only reachable via a hand-edited `settings.toml`, or a
    /// custom theme file that's since been deleted) compiles to `None`, i.e. Jerry Dark, rather
    /// than leaving the previous theme's palette installed unnoticed.
    ///
    /// A real compile *error* (a `base` chain that loops, or names a theme that isn't loaded) is
    /// logged and falls back to Jerry Dark rather than being silently ignored or panicking. That
    /// path is a backstop, not the primary reporting surface:
    /// `custom_theme::load_custom_themes_from_dir` already checks every theme's chain when it
    /// loads the directory and surfaces a real, per-file error on the Themes page, so a broken
    /// chain is normally rejected before it can ever be selected.
    pub(crate) fn apply_theme_selection(&self, cx: &mut Context<Self>) {
        let palette = match custom_theme::compile_palette_by_name(
            &self.settings.theme.name,
            &self.custom_themes,
        ) {
            Ok(palette) => palette,
            Err(err) => {
                log::warn!(
                    "couldn't compile the selected theme \"{}\" ({err}) - falling back to \
                         Jerry Dark",
                    self.settings.theme.name
                );
                None
            }
        };
        theme::set_current_theme(palette.map(std::rc::Rc::new));
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

    /// GitHub issue #141: a real native file picker for a downloaded VSCode theme JSON file,
    /// converted (`vscode_theme::import_vscode_theme_file`) into this app's own five-swatch
    /// format and reloaded from disk - the exact same background-executor
    /// write-then-reload-from-disk shape [`Self::start_import_custom_theme`] already uses for a
    /// plain-TOML source, sharing [`Self::apply_custom_theme_load_result`] with it rather than a
    /// second, parallel applier (see that function's own docs on why it's generic over its error
    /// type to make this possible).
    pub(in crate::settings) fn start_import_vscode_theme(&mut self, cx: &mut Context<Self>) {
        let paths_receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
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
            let result: Result<_, vscode_theme::VscodeImportError> = cx
                .background_executor()
                .spawn(async move {
                    let imported = vscode_theme::import_vscode_theme_file(&source_path, &dest_dir)?;
                    let (themes, errors) = custom_theme::load_custom_themes_from_dir(&dest_dir);
                    Ok((imported, themes, errors))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_custom_theme_load_result(
                    result,
                    |name| format!("Imported \"{name}\" from a VSCode theme."),
                    cx,
                );
            });
        });
        self._vscode_theme_import_task = Some(task);
    }

    /// GitHub issue #5's "custom icon packs": a real native directory picker
    /// (`gpui::App::prompt_for_paths`, `directories: true`) - unlike
    /// [`Self::start_import_custom_theme`], nothing is copied anywhere: the chosen directory
    /// itself becomes [`settings_store::IconPackSettings::directory`] directly, and
    /// `crate::icon_pack::resolve_icon` reads straight out of it at render time, so the user's
    /// own files stay exactly where they left them (editable/replaceable in place, no stale
    /// copy for a later edit to silently miss).
    pub(in crate::settings) fn start_choose_icon_pack_folder(&mut self, cx: &mut Context<Self>) {
        let paths_receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        let task = cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = paths_receiver.await else {
                return;
            };
            let Some(directory) = paths.pop() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.settings.icon_pack.directory = Some(directory.clone());
                this.icon_pack_status = Some(Ok(format!("Using {}.", directory.display())));
                this.persist_settings(cx);
                cx.notify();
            });
        });
        self._icon_pack_choose_task = Some(task);
    }

    /// Real, immediate "back to the app's own default icons" - no confirmation needed, unlike
    /// [`Self::request_remove_custom_theme`]: clearing this setting deletes nothing on disk, it
    /// only stops one persisted path from being read, so there is no real data-loss risk to
    /// guard against the way an actual file deletion has.
    pub(in crate::settings) fn clear_icon_pack(&mut self, cx: &mut Context<Self>) {
        self.settings.icon_pack.directory = None;
        self.icon_pack_status = Some(Ok("Using the app's default icons.".to_string()));
        self.persist_settings(cx);
        cx.notify();
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
    /// Generic over its error type (anything `Display`, not just [`custom_theme::ThemeFileError`])
    /// so `Self::start_import_vscode_theme`'s own [`vscode_theme::VscodeImportError`] - a genuinely
    /// different error domain (a JSON conversion failure has no matching `ThemeFileError` variant
    /// to force it into) - can share this exact same applier rather than a second, parallel one.
    #[allow(clippy::type_complexity)]
    fn apply_custom_theme_load_result<E: std::fmt::Display>(
        &mut self,
        result: Result<
            (
                custom_theme::CustomTheme,
                Vec<custom_theme::CustomTheme>,
                Vec<String>,
            ),
            E,
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
        let Some(active) = self.theme_by_name(&active_name).cloned() else {
            self.custom_theme_status = Some(Err(format!(
                "can't export: no theme named \"{active_name}\" is currently loaded"
            )));
            cx.notify();
            return;
        };
        let export_name = export_theme_name_for(active_name.as_str());
        // Exports the theme's own real file contents - its `base`, its explicit preview swatches
        // and every token it actually names - not a lossy summary of them, so a re-import is a
        // genuine round trip. Jerry Dark is the one theme this produces a nearly-empty file for,
        // and honestly so: it overrides nothing, because it *is* the compiled default palette.
        let export_theme = custom_theme::CustomTheme {
            name: export_name.clone(),
            source_path: None,
            ..active
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

/// The Settings › Terminal shell field's own [`TextFieldHandle`] - what click/drag selection and
/// GitHub issue #336's four clipboard/select-all actions act on, carrying exactly the follow-up
/// work `AdeApp::handle_settings_shell_key_down` already runs after a keystroke: the new value is
/// persisted immediately, and the suggestion dropdown is re-opened against it.
fn shell_input_handle() -> TextFieldHandle {
    TextFieldHandle::new(|app: &mut AdeApp| Some(&mut app.shell_input)).on_changed(
        |app: &mut AdeApp, cx| {
            app.apply_shell_input(cx);
            app.open_shell_suggestions(cx);
        },
    )
}

/// The Settings › Keybindings filter field's own handle. No `on_changed`: the filtered list is
/// derived from the field at render time, so a `cx.notify()` - which every caller of this already
/// does - is the whole of the follow-up work.
fn settings_keymap_filter_handle() -> TextFieldHandle {
    TextFieldHandle::new(|app: &mut AdeApp| Some(&mut app.settings_keymap_filter))
}

/// The "Generate from colour" seed field's own handle. No `on_changed` for the same reason
/// [`settings_keymap_filter_handle`] has none: the live colour swatch beside the field is derived
/// from it at render time, so `cx.notify()` is all the follow-up there is.
fn theme_seed_input_handle() -> TextFieldHandle {
    TextFieldHandle::new(|app: &mut AdeApp| Some(&mut app.theme_seed_input))
}

/// Parses the "Generate from colour" seed input's own text into a real `0xrrggbb` value -
/// tolerant of a missing leading `#` and of surrounding whitespace (both are things a user
/// genuinely types or pastes), but of nothing else: a three-digit shorthand, an alpha channel or
/// a colour *name* would each be a guess about what was meant, and this action's whole output is
/// built around getting that one colour right. A pure function so the parsing rule is directly
/// unit-testable without a window.
fn parse_seed_hex(input: &str) -> Option<u32> {
    let trimmed = input.trim();
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// The real, complete theme file [`AdeApp::start_generate_theme_from_seed`] writes for `seed` -
/// a pure function, so what "generate from colour" actually produces is directly testable without
/// a window, a file dialog or a disk.
///
/// Named `"Custom #rrggbb"` after the seed itself: unique per seed (so generating from two
/// different colours produces two themes rather than silently overwriting one), never colliding
/// with a built-in name, and immediately obvious on the Themes page. Its five card preview
/// swatches are read straight out of the derived palette's own
/// `surface.window`/`surface.rail`/`status.review`/`status.ask`/`status.run`, so the card really
/// shows what the theme looks like.
fn generated_theme_file_for_seed(seed: u32) -> custom_theme::CustomThemeFile {
    let shift = theme::shift_from_seed(theme::hex_rgba(seed));
    let palette = theme::derived_palette(shift);
    let swatch = |key: &str| -> u32 {
        palette
            .iter()
            .find(|(entry_key, _)| *entry_key == key)
            .map(|(_, color)| custom_theme::rgba_to_hex(*color))
            .unwrap_or(seed)
    };
    let preview = [
        swatch("surface.window"),
        swatch("surface.rail"),
        swatch("status.review"),
        swatch("status.ask"),
        swatch("status.run"),
    ];
    crate::settings::builtin_themes::generated_theme_file(
        &format!("Custom #{seed:06x}"),
        "generated from a single colour",
        preview,
        palette,
    )
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
mod theme_seed_tests {
    use super::*;

    #[test]
    fn parse_seed_hex_accepts_the_real_shapes_a_user_types_and_rejects_guesses() {
        assert_eq!(parse_seed_hex("#e07a5f"), Some(0xe07a5f));
        assert_eq!(
            parse_seed_hex("e07a5f"),
            Some(0xe07a5f),
            "a missing # is real input"
        );
        assert_eq!(parse_seed_hex("  #E07A5F  "), Some(0xe07a5f));
        for bad in ["", "#abc", "#e07a5fff", "coral", "#gggggg", "#e07a5"] {
            assert_eq!(parse_seed_hex(bad), None, "{bad:?} should not parse");
        }
    }

    /// The generated file is a real, complete, valid theme - every registered token named, a real
    /// base, real preview swatches - not a stub.
    #[test]
    fn a_generated_seed_theme_is_a_real_complete_valid_palette() {
        let file = generated_theme_file_for_seed(0xe07a5f);
        assert_eq!(file.name, "Custom #e07a5f");
        assert_eq!(file.base.as_deref(), Some("Jerry Dark"));
        assert_eq!(file.overrides.len(), theme::all_tokens().count());
        let validated = file.validate().expect("a generated theme must validate");
        assert_eq!(validated.overrides.len(), theme::all_tokens().count());
        let reparsed = custom_theme::parse_theme_file_str(&validated.to_toml_string())
            .expect("a generated theme must round-trip through its own file format");
        assert_eq!(reparsed.overrides, validated.overrides);
    }

    /// Two different seeds really produce two different palettes and two different names - the
    /// real proof the seed is doing something, and that generating twice doesn't overwrite.
    #[test]
    fn two_different_seeds_produce_two_genuinely_different_themes() {
        let warm = generated_theme_file_for_seed(0xe07a5f)
            .validate()
            .expect("valid");
        let cool = generated_theme_file_for_seed(0x3f7a52)
            .validate()
            .expect("valid");
        assert_ne!(warm.name, cool.name);
        assert_ne!(
            warm.overrides["status.run"], cool.overrides["status.run"],
            "two seeds must really move the palette differently"
        );
    }

    /// The seed's own hue really is where the app's accent lands - the documented contract of
    /// `crate::theme::shift_from_seed`, checked here through the whole file-building path rather
    /// than only at the maths layer.
    #[test]
    fn the_generated_theme_puts_the_apps_accent_on_the_seeds_own_hue() {
        let generated = generated_theme_file_for_seed(0xe07a5f)
            .validate()
            .expect("valid");
        let accent: gpui::Hsla = generated.overrides["syntax.function"].into();
        let seed: gpui::Hsla = theme::hex_rgba(0xe07a5f).into();
        assert!(
            (accent.h - seed.h).abs() < 0.01,
            "the accent hue ({}) should be the seed's own ({})",
            accent.h,
            seed.h
        );
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
            base: Some("Jerry Dark".to_string()),
            preview: None,
            overrides: vec![("surface.window".to_string(), "#0d1117".to_string())],
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
            app.new_agent(ProcessKind::Shell, window, cx);
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

/// GitHub issue #213 ("Allow to select shell") end-to-end: the persisted `terminal.shell` isn't
/// just a string in a file, it decides which real program a real Shell tab's real child process
/// is. Every test here drives the same path a user does - a real window, a real settings value or
/// real keystrokes into the real field, a real spawned pty - rather than asserting on the pure
/// helper alone (which `terminal::pane::shell_program_tests` already covers).
///
/// unix-only: these spawn `sh`, matching this project's own convention of only running the test
/// suite on Linux (`pty-core`'s "Platform scope" docs).
#[cfg(all(test, unix))]
mod shell_setting_tests {
    use super::*;
    use crate::rail::status::Status;
    use crate::root::focus::palette_focus_tests;
    use crate::terminal::pane::TerminalSpec;
    use gpui::TestAppContext;

    /// Same shape as the sibling test modules' own helper (`keybinding_rebind_tests`,
    /// `custom_theme_settings_tests`) - a real window with a real, isolated `settings.toml` path,
    /// so `persist_settings` actually writes a file these tests can read back.
    fn open_test_app_with_real_settings_path(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings: settings_store::Settings,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings,
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    fn settings_with_shell(shell: Option<&str>) -> settings_store::Settings {
        let mut settings = settings_store::Settings::default();
        settings.terminal.shell = shell.map(str::to_string);
        settings
    }

    /// The whole point of the issue: a configured shell is what a real Shell tab really runs.
    /// `sh` rather than the machine's own `$SHELL` so the assertion means something on any host
    /// (a developer already using `sh` as their login shell would make a `$SHELL` comparison
    /// vacuous).
    #[gpui::test]
    fn a_configured_shell_is_what_a_real_shell_tab_really_spawns(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_with_shell(Some("sh")),
            settings_dir.path().join("settings.toml"),
        );
        cx.run_until_parked();

        let pane = app
            .read_with(cx, |app, _| app.agents.active().map(|a| a.pane.clone()))
            .expect("a fresh window starts one real shell tab");

        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.program_label(),
                "sh",
                "the configured shell - not $SHELL - must be the program this tab spawned"
            );
            assert_eq!(
                pane.spawn_error(),
                None,
                "a real, installed shell must start cleanly"
            );
            assert!(
                pane.is_running(),
                "the configured shell must be a genuinely live child process, not just a spec"
            );
        });
    }

    /// The zero-config guarantee: an install that never touches this setting keeps spawning
    /// exactly what it spawned before the setting existed.
    #[gpui::test]
    fn an_unconfigured_shell_still_spawns_the_real_os_default(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let expected = PathBuf::from(TerminalSpec::default_shell_program_display())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .expect("the OS default shell must have a real file name");

        let pane = app
            .read_with(cx, |app, _| app.agents.active().map(|a| a.pane.clone()))
            .expect("a fresh window starts one real shell tab");
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.program_label(), expected);
            assert!(pane.is_running(), "the OS default shell must really run");
        });
    }

    /// GitHub issue #213's honest-failure question: a typo'd shell name must fail the way any
    /// other missing program already does - a real, typed spawn error, named on the tab itself
    /// and reflected in the tab's real status - not a blank pane, and not a silently substituted
    /// fallback that would hide the user's mistake.
    #[gpui::test]
    fn a_misconfigured_shell_fails_visibly_on_the_tab_itself(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_with_shell(Some("definitely-not-a-real-shell-xyz")),
            settings_dir.path().join("settings.toml"),
        );
        cx.run_until_parked();

        let pane = app
            .read_with(cx, |app, _| app.agents.active().map(|a| a.pane.clone()))
            .expect("the tab is still created - it is the *process* that failed");
        let error = pane
            .read_with(cx, |pane, _| pane.spawn_error().map(str::to_string))
            .expect("a shell that doesn't exist must record a real spawn error");
        assert!(
            error.contains("definitely-not-a-real-shell-xyz"),
            "the error must name the program the user actually configured, so the typo is \
             diagnosable: {error}"
        );

        let status = app.read_with(cx, |app, cx| {
            let agent = app.agents.active().expect("the failed tab");
            app.agent_status(agent, cx)
        });
        assert_eq!(
            status,
            Status::Fail,
            "a tab whose shell never started must read as a real failure everywhere the app \
             shows status, not as idle"
        );

        // And the Settings row says so up front, before the user has to read a terminal.
        app.update(cx, |app, _| app.refresh_shell_status());
        assert!(
            app.read_with(cx, |app, _| app.shell_status.is_not_found()),
            "the Settings row must flag a shell that genuinely isn't there"
        );
    }

    /// The real user journey, driven through the real UI: focus the real painted field, type a
    /// real shell name, and it lands in the real `settings.toml` *and* in the next real tab's
    /// real child process.
    #[gpui::test]
    fn typing_a_shell_into_the_settings_row_persists_it_and_the_next_tab_uses_it(
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
        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::General, window, cx);
        });
        cx.run_until_parked();

        let field = cx
            .debug_bounds("settings-shell-input")
            .expect("the Shell field must really paint on the General page");
        cx.simulate_click(field.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("s h");
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.terminal.shell.clone()),
            Some("sh".to_string()),
            "real keystrokes in the real field must reach the real setting"
        );
        let written = std::fs::read_to_string(&settings_path).expect("the file must exist");
        assert!(
            written.contains("shell = \"sh\""),
            "the edit must really be persisted to settings.toml, got: {written}"
        );

        // The next real Shell tab runs it - and really starts.
        app.update_in(cx, |app, window, cx| {
            app.close_settings(window, cx);
            app.new_agent(ProcessKind::Shell, window, cx);
        });
        cx.run_until_parked();
        let pane = app
            .read_with(cx, |app, _| app.agents.active().map(|a| a.pane.clone()))
            .expect("the newly spawned tab");
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.program_label(), "sh");
            assert_eq!(pane.spawn_error(), None);
            assert!(pane.is_running(), "the typed shell must really be running");
        });

        // Clearing the field again is a real edit back to the system default, not a stuck value.
        app.update_in(cx, |app, window, cx| {
            app.open_settings(window, cx);
            app.select_settings_page(SettingsPage::General, window, cx);
        });
        cx.run_until_parked();
        let field = cx
            .debug_bounds("settings-shell-input")
            .expect("the Shell field must paint again on reopening Settings");
        cx.simulate_click(field.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.terminal.shell.clone()),
            None,
            "emptying the field must mean 'use the system default', not an empty program name"
        );
    }

    /// A shell configured in the file shows up in the field the moment Settings is opened -
    /// otherwise a user with a hand-edited `settings.toml` would see a blank field and assume
    /// nothing was set.
    #[gpui::test]
    fn an_already_persisted_shell_is_shown_in_the_field_at_startup(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_with_shell(Some("sh")),
            settings_dir.path().join("settings.toml"),
        );
        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::General, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.shell_input.as_str().to_string()),
            "sh",
            "the field must start out holding the real persisted value"
        );
        assert!(
            app.read_with(cx, |app, _| matches!(
                app.shell_status,
                settings::ShellStatus::Resolved(_)
            )),
            "opening Settings must re-probe the configured shell against the real PATH"
        );
        assert!(
            cx.debug_bounds("settings-shell-status").is_some(),
            "the row's live found/not-found hint must really paint"
        );
    }
}

/// GitHub issue #213's follow-up ("would a select + auto-detect installed shells be better?" -
/// answered as a hybrid): real detected shells offered as clickable suggestions under a field that
/// stays unrestricted free text.
///
/// Every test here drives the real thing end to end - a real window, real painted bounds, a real
/// mouse click on a real suggestion row, the real `/etc/shells`/`PATH` detection of the machine
/// running the suite - rather than asserting on the pure detector alone (which
/// `crate::settings::state`'s own tests already cover against real tempdir fixtures).
///
/// unix-only, like its sibling module: the final assertions spawn a real shell.
#[cfg(all(test, unix))]
mod shell_suggestion_dropdown_tests {
    use super::*;
    use crate::root::menus::MenuSurface;
    use gpui::TestAppContext;

    /// Opens Settings on the General page, with a real isolated `settings.toml`, and returns the
    /// app plus that path.
    fn open_general_settings(
        cx: &mut TestAppContext,
        settings_path: PathBuf,
        repo_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        let (app, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });
        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::General, window, cx);
        });
        cx.run_until_parked();
        (app, cx)
    }

    /// `VisualTestContext::debug_bounds` takes a `&'static str`, and a suggestion row's selector
    /// is built from its runtime position in the filtered list - so a test that wants to click
    /// row *n* has to hand out a genuinely `'static` selector for it. Leaking a per-lookup
    /// `String` is the honest way to do that in a test binary: bounded (a handful of rows per
    /// test), and it keeps the row selectors index-based, which is what makes them unique even on
    /// a machine with two shells of the same name.
    fn row_selector(index: usize) -> &'static str {
        Box::leak(format!("settings-shell-suggestion-{index}").into_boxed_str())
    }

    /// Clicks the real painted Shell field, which is what opens the dropdown.
    fn click_the_shell_field(cx: &mut gpui::VisualTestContext) {
        let field = cx
            .debug_bounds("settings-shell-input")
            .expect("the Shell field must really paint on the General page");
        cx.simulate_click(field.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    /// The core of the feature: clicking the field really paints a dropdown, and every row in it
    /// is a shell that genuinely exists on this machine - detected, not hardcoded.
    #[gpui::test]
    fn clicking_the_field_opens_a_dropdown_of_genuinely_detected_shells(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_general_settings(
            cx,
            settings_dir.path().join("settings.toml"),
            repo.path().to_path_buf(),
        );

        assert!(
            cx.debug_bounds("settings-shell-suggestions-popover")
                .is_none(),
            "the dropdown must not be painted before anything has been clicked"
        );

        click_the_shell_field(cx);

        assert!(
            app.read_with(cx, |app, _| app.shell_suggestions_open),
            "clicking the field must really open the suggestion surface"
        );
        assert!(
            cx.debug_bounds("settings-shell-suggestions-popover")
                .is_some(),
            "the dropdown must really paint, with real bounds, under the field"
        );

        let suggestions = app.read_with(cx, |app, _| app.shell_suggestions.clone());
        assert!(
            !suggestions.is_empty(),
            "detection must find real shells on a machine that genuinely has /etc/shells"
        );
        for (index, suggestion) in suggestions.iter().enumerate() {
            assert!(
                suggestion.path.is_file(),
                "the dropdown offered {}, which is not a real file on this machine",
                suggestion.path.display()
            );
            assert!(
                cx.debug_bounds(row_selector(index)).is_some(),
                "every detected shell must really paint as a clickable row: {}",
                suggestion.name
            );
        }
    }

    /// The whole user journey the follow-up asked for, with no typing at all: click the field,
    /// click a real detected shell, and that shell's real path is in the field, in the real
    /// `settings.toml`, and running as the real child process of the next Shell tab.
    #[gpui::test]
    fn clicking_a_suggestion_configures_it_and_the_next_tab_really_runs_it(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let (app, cx) = open_general_settings(cx, settings_path.clone(), repo.path().to_path_buf());
        click_the_shell_field(cx);

        // Pick a real detected shell this test can then really spawn. `sh` is the one every Unix
        // host running this suite genuinely has.
        let (index, chosen) = app
            .read_with(cx, |app, _| {
                app.shell_suggestions
                    .iter()
                    .enumerate()
                    .find(|(_, suggestion)| suggestion.name == "sh")
                    .map(|(index, suggestion)| (index, suggestion.clone()))
            })
            .expect("every Unix host running this suite has a real sh in /etc/shells");

        let row = cx
            .debug_bounds(row_selector(index))
            .expect("the chosen suggestion must really paint");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.shell_input.as_str().to_string()),
            chosen.value(),
            "clicking a suggestion must put its real path in the field, exactly as typing would"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.terminal.shell.clone()),
            Some(chosen.value()),
            "and it must reach the real setting through the same path a typed value does"
        );
        let written = std::fs::read_to_string(&settings_path).expect("the file must exist");
        assert!(
            written.contains(&format!("shell = \"{}\"", chosen.value())),
            "a clicked suggestion must really be persisted to settings.toml, got: {written}"
        );
        assert!(
            !app.read_with(cx, |app, _| app.shell_suggestions_open),
            "picking a suggestion must close the dropdown, not leave it stuck open"
        );
        assert!(
            cx.debug_bounds("settings-shell-suggestions-popover")
                .is_none(),
            "and it must really stop painting"
        );

        // The real proof it was a usable choice and not just a string: the next Shell tab spawns it.
        app.update_in(cx, |app, window, cx| {
            app.close_settings(window, cx);
            app.new_agent(ProcessKind::Shell, window, cx);
        });
        cx.run_until_parked();
        let pane = app
            .read_with(cx, |app, _| app.agents.active().map(|a| a.pane.clone()))
            .expect("the newly spawned tab");
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.spawn_error(),
                None,
                "a shell picked from the detected list must always start - that is what makes \
                 detection worth trusting"
            );
            assert_eq!(pane.program_label(), "sh");
            assert!(pane.is_running(), "the picked shell must really be running");
        });
    }

    /// The field must stay genuinely free text: a path this machine has never heard of is still
    /// typeable and still persists, and the dropdown - which has nothing to suggest for it -
    /// simply shows no rows rather than restricting or overriding anything.
    #[gpui::test]
    fn a_custom_value_the_machine_has_never_heard_of_still_works(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_general_settings(
            cx,
            settings_dir.path().join("settings.toml"),
            repo.path().to_path_buf(),
        );
        click_the_shell_field(cx);
        cx.simulate_keystrokes("q q q");
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.terminal.shell.clone()),
            Some("qqq".to_string()),
            "typing must reach the setting exactly as it did before the dropdown existed"
        );
        assert!(
            app.read_with(cx, |app, _| settings::filter_shell_suggestions(
                &app.shell_suggestions,
                app.shell_input.as_str()
            )
            .is_empty()),
            "nothing detected matches a custom value, so nothing may be suggested for it"
        );
        assert!(
            cx.debug_bounds("settings-shell-suggestion-0").is_none(),
            "no suggestion row may paint when nothing matches - the dropdown never restricts what \
             can be typed, it just has nothing to add"
        );

        // And typing something the machine *does* have narrows the list to it rather than
        // replacing what was typed.
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes("z s h");
        cx.run_until_parked();
        let matched = app.read_with(cx, |app, _| {
            settings::filter_shell_suggestions(&app.shell_suggestions, app.shell_input.as_str())
                .into_iter()
                .map(|suggestion| suggestion.name.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.shell_input.as_str().to_string()),
            "zsh",
            "the field still holds exactly what was typed - a suggestion never rewrites it"
        );
        if app.read_with(cx, |app, _| {
            app.shell_suggestions
                .iter()
                .any(|suggestion| suggestion.name == "zsh")
        }) {
            assert_eq!(
                matched,
                vec!["zsh".to_string()],
                "typing a real shell's name must narrow the list to it"
            );
        }
    }

    /// A clicked suggestion is an ordinary edit of the same [`crate::text_history::TextField`] the
    /// field already used, so the field's existing undo history really covers it - a real
    /// regression guard against the dropdown growing a second, parallel way to change the value
    /// that undo would not know about.
    #[gpui::test]
    fn a_clicked_suggestion_is_a_single_undoable_edit_of_the_real_field(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_general_settings(
            cx,
            settings_dir.path().join("settings.toml"),
            repo.path().to_path_buf(),
        );
        click_the_shell_field(cx);
        // A real typed value first, so undo has something distinct to come back to.
        cx.simulate_keystrokes("s");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.shell_input.as_str().to_string()),
            "s"
        );

        let row = cx
            .debug_bounds("settings-shell-suggestion-0")
            .expect("'s' matches real detected shells, so a row must paint");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let picked = app.read_with(cx, |app, _| app.shell_input.as_str().to_string());
        assert!(
            picked.starts_with('/'),
            "the click must have replaced the typed text with a real absolute path, got {picked}"
        );

        let undo = if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        };
        cx.simulate_keystrokes(undo);
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.shell_input.as_str().to_string()),
            "s",
            "the field's own existing undo must reverse a clicked suggestion in one step - the \
             dropdown writes through the same TextField, not around it"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.terminal.shell.clone()),
            Some("s".to_string()),
            "and the undone value must be re-applied to the real setting, so the field and the \
             file can never disagree"
        );
    }

    /// The dropdown obeys the app's one shared dismissal rule
    /// (`crate::root::menus::MenuSurface`), rather than owning a second one: a click away closes
    /// it, the window losing focus closes it, and leaving Settings closes it.
    #[gpui::test]
    fn the_dropdown_dismisses_the_same_way_every_other_menu_does(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_general_settings(
            cx,
            settings_dir.path().join("settings.toml"),
            repo.path().to_path_buf(),
        );

        // It is a real member of the shared menu-surface set, not a private bool.
        click_the_shell_field(cx);
        assert!(
            app.read_with(cx, |app, _| app
                .menu_surface_is_open(MenuSurface::ShellSuggestions)),
            "the dropdown must answer the shared 'is a menu open' question"
        );

        // A click away from the panel hits the scrim and closes it.
        let popover = cx
            .debug_bounds("settings-shell-suggestions-popover")
            .expect("the dropdown must be painted");
        cx.simulate_click(
            gpui::Point::new(popover.origin.x - px(80.0), popover.origin.y + px(200.0)),
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.shell_suggestions_open),
            "clicking away from the dropdown must close it"
        );

        // Leaving Settings entirely never leaves it armed for the next visit.
        click_the_shell_field(cx);
        assert!(app.read_with(cx, |app, _| app.shell_suggestions_open));
        app.update_in(cx, |app, window, cx| app.close_settings(window, cx));
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.shell_suggestions_open),
            "closing Settings must close the dropdown that belonged to it"
        );
        app.update_in(cx, |app, window, cx| {
            app.open_settings(window, cx);
            app.select_settings_page(SettingsPage::General, window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("settings-shell-suggestions-popover")
                .is_none(),
            "reopening Settings must not resurrect a dropdown nobody asked for"
        );

        // And the window losing OS focus closes it, exactly like every other menu surface - last,
        // since `VisualTestContext` has no way to re-activate a window afterwards.
        //
        // `deactivate_window` is a no-op unless this window really is the platform's active one,
        // and `TestWindow` does not start active - the same premise
        // `crate::root::menus`'s own window-activation test sets up.
        cx.update(|window, _cx| window.activate_window());
        cx.run_until_parked();
        click_the_shell_field(cx);
        assert!(app.read_with(cx, |app, _| app.shell_suggestions_open));
        cx.deactivate_window();
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.shell_suggestions_open),
            "a deactivated window must not leave a dropdown floating over the app"
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

/// GitHub issue #122 ("Add settings to display indents in code editor") - real coverage for
/// [`AdeApp::toggle_indent_guides`], mirroring `caret_settings_tests`' own established "through
/// the real mutator, assert it actually persisted" discipline. The real *painted* effect of this
/// setting (whether a guide line actually appears/disappears in the File view) is covered
/// separately by `crate::code_surface::editing::indent_guide_tests`, which is the one that would
/// actually catch a toggle that flips the field but changes nothing on screen.
#[cfg(test)]
mod indent_guide_settings_tests {
    use crate::root::AdeApp;
    use crate::settings::store as settings_store;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    /// A real, temp-dir-scoped settings path - mirrors `crate::sidebar::render::fold_state_tests`'
    /// own `open_app_with_state_dir` (that one isn't `pub(crate)`, so this is a small, deliberate
    /// duplicate rather than a cross-module dependency on a test-only helper), which is what gives
    /// this test a real `settings.toml` to persist to and reload from, unlike
    /// `crate::root::focus::palette_focus_tests::open_test_app`'s deliberately unpersisted `None`.
    ///
    /// Loads real settings from `settings_path` first (via `Settings::load_or_init_at`, the same
    /// real load `crate::root::mod`'s own startup path uses) rather than always constructing with
    /// `Settings::default()` - see `keybinding_rebind_tests::
    /// open_test_app_with_real_settings_path`'s identical real-load-before-construct pattern, one
    /// call site up in this same file. Passing a fixed `Settings::default()` regardless of what's
    /// really on disk would make every "reload" in this module a no-op standing in for nothing -
    /// a real, live-caught bug in this test module's own first draft (a "reload" that never
    /// re-reads its own file can never fail no matter what persistence bug it's meant to catch).
    fn open_app_with_state_dir(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        let settings = settings_store::Settings::load_or_init_at(&settings_path);
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings,
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    fn toggle_indent_guides_flips_the_real_persisted_setting_and_persists_across_reload(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());

        assert!(
            app.read_with(cx, |app, _| app.settings.appearance.show_indent_guides),
            "sanity check: the real default is guides on"
        );

        app.update(cx, |app, cx| {
            app.toggle_indent_guides(cx);
        });
        assert!(
            !app.read_with(cx, |app, _| app.settings.appearance.show_indent_guides),
            "the real Settings field must have flipped off"
        );
        // `persist_settings` writes asynchronously through the real serial writer task (see
        // `AdeApp::_settings_save_task`'s own docs) - the write must actually land on disk before
        // reopening from the same path below, or this test just proves the in-memory flip, which
        // the assertion right above it already covers.
        cx.run_until_parked();

        let (reloaded, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        assert!(
            !reloaded.read_with(cx, |app, _| app.settings.appearance.show_indent_guides),
            "the toggle must have really been persisted to disk, not just flipped in memory"
        );

        app.update(cx, |app, cx| {
            app.toggle_indent_guides(cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.settings.appearance.show_indent_guides),
            "the real Settings field must have flipped back on"
        );
    }
}

/// GitHub issue #45 ("Input blink only on focused input or file") plus a live follow-up report:
/// [`AdeApp::render_settings_keymap_filter_row`]'s caret used to be a fixed trailing child,
/// painted *after* the placeholder text whenever `settings_keymap_filter` was empty, instead of
/// at the real cursor position (0). Real interaction coverage, mirroring
/// `crate::palette::render::palette_caret_tests`'/`crate::rail::render::rail_filter_caret_tests`'
/// own measured-bounds technique rather than only reading the render code.
#[cfg(test)]
mod settings_keymap_filter_caret_tests {
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::time::Duration;

    #[gpui::test]
    fn caret_sits_before_the_placeholder_when_empty_and_after_the_text_once_typed(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_settings(window, cx);
            app.settings_page = crate::settings::SettingsPage::Keymap;
            window.focus(&app.settings_keymap_filter_focus_handle, cx);
        });
        cx.run_until_parked();

        let empty_caret = cx
            .debug_bounds("settings-keymap-filter-caret")
            .expect("the caret should have really painted with an empty filter");
        let placeholder = cx
            .debug_bounds("settings-keymap-filter-text")
            .expect("the placeholder text should have really painted");
        assert!(
            empty_caret.origin.x <= placeholder.origin.x,
            "with an empty filter, the real caret must sit before (at or left of) the \
             placeholder's own start x, not after it - got caret {:?} vs placeholder {:?}",
            empty_caret,
            placeholder,
        );

        cx.simulate_input("palette");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_keymap_filter.as_str().to_string()),
            "palette",
            "sanity check: real typed filter"
        );

        let typed_caret = cx
            .debug_bounds("settings-keymap-filter-caret")
            .expect("the caret should have really painted with a typed filter");
        let typed_text = cx
            .debug_bounds("settings-keymap-filter-text")
            .expect("the real typed text should have really painted");
        assert!(
            typed_caret.origin.x >= typed_text.origin.x + typed_text.size.width,
            "with a typed filter, the real caret must sit at or after the typed text's own \
             right edge, not before it - got caret {:?} vs text {:?}",
            typed_caret,
            typed_text,
        );
        assert!(
            typed_caret.origin.x > empty_caret.origin.x,
            "the caret's real measured horizontal position must differ between the \
             empty-filter state (before the placeholder) and a typed-filter state (after the \
             real text) - got {:?} vs {:?}",
            empty_caret.origin.x,
            typed_caret.origin.x,
        );
    }

    /// **The second live instance of the same structural caret bug**, found by auditing every
    /// hand-rolled input in this app after the review-note card's own report (*"Caret is not
    /// right, does not follow the typing of the user and just stays on the right side"*).
    ///
    /// This field used to carry `.flex_1().min_w_0()` on its **text** element. The field itself is
    /// a fixed 168px box, so the text's layout box filled the whole of it whatever the shell path
    /// said, and the caret - a `flex_none` sibling *after* that box - was pinned against the
    /// field's right-hand border rather than sitting after the last character typed.
    ///
    /// Both this field and the review-note card now build their caret+text row through
    /// `AdeApp::render_simple_input_row`, which is where that placement lives now.
    #[gpui::test]
    fn the_shell_fields_caret_follows_the_text_rather_than_pinning_to_the_fields_edge(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_settings(window, cx);
            app.settings_page = crate::settings::SettingsPage::General;
            window.focus(&app.shell_focus_handle, cx);
        });
        cx.run_until_parked();

        let field = cx
            .debug_bounds("settings-shell-input")
            .expect("the shell field must really paint");
        let empty_caret = cx
            .debug_bounds("settings-shell-caret")
            .expect("and its caret, with the field empty");
        let placeholder = cx
            .debug_bounds("settings-shell-text")
            .expect("and its placeholder");
        assert!(
            empty_caret.origin.x <= placeholder.origin.x,
            "an empty field's real cursor position is 0 - got caret {empty_caret:?} vs \
             placeholder {placeholder:?}"
        );

        cx.simulate_input("/bin/sh");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.shell_input.as_str().to_string()),
            "/bin/sh",
            "sanity check: the field really took the typed text"
        );

        let text = cx
            .debug_bounds("settings-shell-text")
            .expect("the typed text must really paint");
        let caret = cx
            .debug_bounds("settings-shell-caret")
            .expect("and so must its caret");
        assert!(
            caret.origin.x >= text.origin.x + text.size.width - gpui::px(1.0)
                && caret.origin.x < text.origin.x + text.size.width + gpui::px(4.0),
            "the caret must sit flush against the last glyph - got caret {caret:?} vs text {text:?}"
        );
        assert!(
            caret.origin.x + caret.size.width < field.origin.x + field.size.width - gpui::px(20.0),
            "and nowhere near the field's own right border, which is where a `flex_1` on the text \
             element used to put it - got caret {caret:?} in field {field:?}"
        );
    }

    /// GitHub issue #45's own title, taken literally - see
    /// `crate::rail::render::rail_filter_caret_tests`' identical test for why
    /// `cx.simulate_input` (not a bare `window.focus`) is what actually forces the real redraw
    /// `on_focus` fires from in this test harness.
    #[gpui::test]
    fn focusing_the_settings_keymap_filter_starts_the_real_shared_blink_loop(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        // `on_focus`/`on_blur` (`AdeApp::wire_caret_blink`'s own mechanism) only fire while GPUI
        // considers the window itself "active" - a real, freshly opened test window starts out
        // not active at all.
        app.update_in(cx, |_app, window, _cx| window.activate_window());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_settings(window, cx);
            app.settings_page = crate::settings::SettingsPage::Keymap;
            window.focus(&app.settings_keymap_filter_focus_handle, cx);
        });
        cx.simulate_input("p");
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "a fresh focus must start solid/visible"
        );

        cx.background_executor.advance_clock(
            crate::root::caret_blink::CARET_BLINK_INTERVAL + Duration::from_millis(50),
        );
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.caret_blink_visible),
            "focusing the settings keymap filter must have started the real, live shared blink \
             task"
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
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings,
                Some(settings_path),
                window,
                cx,
            )
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
/// module proves the *wiring* (persistence, the live `crate::theme::CURRENT_THEME` palette
/// install, and that a representative real render call genuinely reads the new value), which a
/// pure unit test of `crate::theme` alone can't catch (e.g. forgetting to call
/// `apply_theme_selection` from `Self::set_theme_name` would still pass every `crate::theme`
/// test).
#[cfg(test)]
mod theme_swap_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    /// `crate::theme::CURRENT_THEME` is real, thread-local, mutable state - reset it after this
    /// test regardless of outcome, matching `crate::theme::theme_runtime_tests`'s own discipline
    /// (see that module's docs for why a leaked palette would corrupt other tests on the same
    /// thread). In practice any *other* test that goes on to construct a fresh `AdeApp` already
    /// self-heals this via `Self::apply_theme_selection` running in `Self::new_with_settings`,
    /// but this test doesn't rely on that - it cleans up its own real write directly.
    struct ResetThemeIndexOnDrop;
    impl Drop for ResetThemeIndexOnDrop {
        fn drop(&mut self) {
            theme::set_current_theme(None);
        }
    }

    #[gpui::test]
    fn selecting_a_real_theme_card_installs_the_live_palette_and_changes_a_representative_color(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeIndexOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        assert!(
            theme::current_theme_palette().is_none(),
            "a fresh app defaults to Jerry Dark, the real no-palette identity case"
        );
        let jerry_dark_window_bg = theme::surface::WINDOW.resolve();

        app.update(cx, |app, cx| {
            app.set_theme_name("Slate".to_string(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Slate",
            "the selection must really persist in Settings"
        );
        let palette = theme::current_theme_palette()
            .expect("selecting a real bundled theme must install a real compiled palette");
        assert_eq!(
            palette.len(),
            theme::all_tokens().count(),
            "Slate is a complete generated palette - every real token should be in it"
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
        assert!(theme::current_theme_palette().is_none());
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
        assert!(
            theme::theme_is_light(theme::surface::WINDOW.resolve()),
            "selecting Paper must really make the live window background a light colour"
        );

        app.update(cx, |app, cx| {
            app.apply_follow_system_appearance(gpui::WindowAppearance::Dark, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Jerry Dark",
            "a real OS-dark signal must switch back to the real last-chosen dark theme, which \
             for a fresh install is the documented default"
        );
        assert!(theme::current_theme_palette().is_none());
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

    /// Same real-leak discipline `theme_swap_tests::ResetThemeIndexOnDrop` documents, for
    /// `crate::theme::CURRENT_THEME` - a custom-theme test can leave a palette installed.
    struct ResetThemeStateOnDrop;
    impl Drop for ResetThemeStateOnDrop {
        fn drop(&mut self) {
            theme::set_current_theme(None);
        }
    }

    fn open_test_app_with_real_settings_path(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings: settings_store::Settings,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings,
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    fn write_custom_theme_file(themes_dir: &std::path::Path, file_name: &str, contents: &str) {
        std::fs::create_dir_all(themes_dir).expect("create themes dir");
        std::fs::write(themes_dir.join(file_name), contents).expect("write theme file");
    }

    /// A real, minimal theme file in the current format - a name, a base, and a handful of real
    /// token keys. Deliberately partial (it names six of the app's ~270 tokens), so these tests
    /// exercise the same "override a little, inherit the rest" shape a hand-authored theme
    /// actually has.
    const MIDNIGHT_CORAL_TOML: &str = "name = \"Midnight Coral\"\n\
         subtitle = \"warm accent\"\n\
         base = \"Jerry Dark\"\n\
         \n\
         [surface]\n\
         window = \"#0c0d10\"\n\
         card = \"#181a1e\"\n\
         rail = \"#101216\"\n\
         \n\
         [status]\n\
         review = \"#5cb87f\"\n\
         ask = \"#e2a336\"\n\
         \n\
         [syntax]\n\
         keyword = \"#e07a5f\"\n";

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

    /// GitHub issue #141: the same real validate-then-apply proof
    /// `importing_a_real_theme_file_adds_it_to_the_registry_and_writes_a_canonical_copy` gives
    /// the plain-TOML path, driven through `vscode_theme::import_vscode_theme_file` and the now-
    /// generic `Self::apply_custom_theme_load_result` instead - a real VSCode theme JSON file on
    /// disk really does become a real, selectable custom theme, and a malformed one is rejected
    /// without touching the registry.
    #[gpui::test]
    fn importing_a_real_vscode_theme_file_adds_it_to_the_registry_and_writes_a_canonical_copy(
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
        let source_path = source_dir.path().join("dracula.json");
        std::fs::write(
            &source_path,
            r##"{
                "name": "Dracula Imported",
                "colors": {
                    "editor.background": "#282a36",
                    "sideBar.background": "#21222c",
                    "terminal.ansiGreen": "#50fa7b",
                    "terminal.ansiYellow": "#f1fa8c",
                    "button.background": "#bd93f9"
                }
            }"##,
        )
        .expect("write source file");
        let dest_dir = settings_dir.path().join("themes");

        let result: Result<_, vscode_theme::VscodeImportError> =
            vscode_theme::import_vscode_theme_file(&source_path, &dest_dir).map(|imported| {
                let (themes, errors) = custom_theme::load_custom_themes_from_dir(&dest_dir);
                (imported, themes, errors)
            });
        app.update(cx, |app, cx| {
            app.apply_custom_theme_load_result(
                result,
                |name| format!("Imported \"{name}\" from a VSCode theme."),
                cx,
            );
        });

        assert_eq!(app.read_with(cx, |app, _| app.custom_themes.len()), 1);
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_themes[0].name.clone()),
            "Dracula Imported"
        );
        let status = app.read_with(cx, |app, _| app.custom_theme_status.clone());
        assert!(
            matches!(status, Some(Ok(_))),
            "a real successful VSCode import should report a real success status, got {status:?}"
        );
        let expected_file = dest_dir.join("dracula-imported.toml");
        assert!(
            expected_file.exists(),
            "import must write a real, canonical TOML copy into the settings-sibling themes \
             directory"
        );

        // A source with no `editor.background` at all is rejected with a real error and does
        // not touch the registry.
        let bad_source = source_dir.path().join("bad.json");
        std::fs::write(&bad_source, r#"{ "colors": {} }"#).expect("write bad source");
        let bad_result: Result<_, vscode_theme::VscodeImportError> =
            vscode_theme::import_vscode_theme_file(&bad_source, &dest_dir).map(|imported| {
                let (themes, errors) = custom_theme::load_custom_themes_from_dir(&dest_dir);
                (imported, themes, errors)
            });
        app.update(cx, |app, cx| {
            app.apply_custom_theme_load_result(
                bad_result,
                |name| format!("Imported \"{name}\" from a VSCode theme."),
                cx,
            );
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_themes.len()),
            1,
            "a VSCode file with no real background colour must not add a bogus entry"
        );
        let status = app.read_with(cx, |app, _| app.custom_theme_status.clone());
        assert!(
            matches!(status, Some(Err(_))),
            "a malformed VSCode import should report a real, honest error, got {status:?}"
        );
    }

    /// GitHub issue #141's "Generate from colour": typing a real hex seed into the Themes page's
    /// own input and clicking Generate must write a real, complete theme file into this instance's
    /// themes directory, load it as a real card, and report a real success - the whole action
    /// end-to-end, driven the way a user drives it (a real focused input receiving real
    /// keystrokes, then a real click on the real painted button), not by calling the pure helper.
    #[gpui::test]
    fn typing_a_seed_colour_and_clicking_generate_really_writes_a_whole_theme_file(
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

        let input_bounds = cx
            .debug_bounds("settings-theme-seed-input")
            .expect("the seed input must have painted");
        cx.simulate_click(input_bounds.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("# e 0 7 a 5 f");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.theme_seed_input.as_str().to_string()),
            "#e07a5f",
            "real keystrokes must reach the real seed field"
        );

        let button_bounds = cx
            .debug_bounds("settings-theme-generate")
            .expect("the Generate button must have painted");
        cx.simulate_click(button_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let status = app.read_with(cx, |app, _| app.custom_theme_status.clone());
        assert!(
            matches!(status, Some(Ok(_))),
            "generating from a real seed colour must report a real success, got {status:?}"
        );
        let themes = app.read_with(cx, |app, _| app.custom_themes.clone());
        assert_eq!(themes.len(), 1, "the generated theme must really be loaded");
        assert_eq!(themes[0].name, "Custom #e07a5f");
        assert_eq!(
            themes[0].overrides.len(),
            theme::all_tokens().count(),
            "a generated theme must be a real, complete palette the user can hand-tune"
        );
        let file = themes[0]
            .source_path
            .clone()
            .expect("the generated theme must have a real backing file");
        assert!(file.exists());
        assert!(
            std::fs::read_to_string(&file)
                .expect("readable")
                .contains("[syntax]"),
            "the written file must really be the grouped, editable format, not an opaque blob"
        );

        // And it is genuinely selectable: picking it really re-skins the app.
        let jerry_dark_window = theme::surface::WINDOW.resolve();
        app.update(cx, |app, cx| {
            app.set_theme_name("Custom #e07a5f".to_string(), cx);
        });
        cx.run_until_parked();
        assert!(
            theme::syntax::KEYWORD.resolve() != jerry_dark_window
                && theme::syntax::KEYWORD.resolve() != theme::syntax::KEYWORD.default,
            "selecting the generated theme must really change what the app resolves"
        );
    }

    /// A malformed seed is a real, specific error - not a silent no-op, and not a guessed default
    /// colour.
    #[gpui::test]
    fn generating_from_a_malformed_seed_is_a_real_reported_error(cx: &mut TestAppContext) {
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

        app.update(cx, |app, cx| {
            app.theme_seed_input.set("nope", std::time::Instant::now());
            app.start_generate_theme_from_seed(cx);
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, _| app.custom_theme_status.clone());
        assert!(
            matches!(status, Some(Err(_))),
            "a malformed seed must report a real error, got {status:?}"
        );
        assert!(
            app.read_with(cx, |app, _| app.custom_themes.is_empty()),
            "nothing should have been created"
        );
    }

    /// The real, click-driven proof the "Import VSCode theme…" button itself is wired to a real
    /// handler - mirrors `clicking_the_real_import_button_reaches_the_real_handler`'s own
    /// discipline for the plain-TOML button.
    #[gpui::test]
    fn clicking_the_real_import_vscode_theme_button_reaches_the_real_handler(
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

        let import_bounds = cx
            .debug_bounds("settings-theme-import-vscode")
            .expect("the Import VSCode theme… button must have painted");
        cx.simulate_click(import_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app._vscode_theme_import_task.is_some()),
            "a real click on the button must actually invoke Self::start_import_vscode_theme, \
             not silently do nothing"
        );
    }

    /// GitHub issue #141: selecting a real, imported VSCode theme with a `[syntax]` table must
    /// really change `code_surface::code_view::color_for_kind`'s *live* output for the buckets
    /// it named - not just update `Self::custom_themes`' own in-memory record. This is the one
    /// real end-to-end proof connecting `Self::apply_theme_selection` to
    /// `crate::theme::set_current_syntax_overrides`, on top of `vscode_theme`'s own pure
    /// conversion tests and `code_view`'s own pure override-precedence test.
    #[gpui::test]
    fn selecting_an_imported_vscode_theme_really_changes_the_live_syntax_colour(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let dest_dir = settings_dir.path().join("themes");
        let source_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("dracula.json");
        std::fs::write(
            &source_path,
            r##"{
                "name": "Dracula Live",
                "colors": { "editor.background": "#282a36", "sideBar.background": "#21222c" },
                "tokenColors": [
                    { "scope": "keyword.control", "settings": { "foreground": "#ff79c6" } }
                ]
            }"##,
        )
        .expect("write source file");
        let imported = vscode_theme::import_vscode_theme_file(&source_path, &dest_dir)
            .expect("import must succeed");

        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path,
        );
        app.update(cx, |app, cx| {
            app.custom_themes = vec![imported];
            app.settings.theme.name = "Dracula Live".to_string();
            app.apply_theme_selection(cx);
        });
        cx.run_until_parked();

        let live_color = crate::code_surface::code_view::color_for_kind(
            crate::code_surface::code_view::HighlightKind::Keyword,
        );
        assert_eq!(
            live_color,
            gpui::Rgba {
                r: 0xff as f32 / 255.0,
                g: 0x79 as f32 / 255.0,
                b: 0xc6 as f32 / 255.0,
                a: 1.0,
            },
            "selecting the imported theme must really change color_for_kind's live output for \
             Keyword to the real colour the VSCode theme's own tokenColors named"
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

/// GitHub issue #168's `appearance.bracket_pair_colorization` toggle. Deliberately split the same
/// way `indent_guide_settings_tests` is: the field/persistence half lives here, and the half that
/// would actually catch "the toggle flips a field but nothing changes" lives next to the real
/// highlighting it gates
/// (`crate::code_surface::code_view::tests::bracket_pair_colorization_setting_tests`) plus the
/// live-invalidation test in `crate::code_surface::editing`.
#[cfg(test)]
mod bracket_pair_colorization_settings_tests {
    use super::*;
    use crate::root::AdeApp;
    use crate::settings::store as settings_store;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    /// Same real-load-before-construct helper `indent_guide_settings_tests` uses - see its own
    /// docs for why loading from disk first is load-bearing rather than incidental.
    fn open_app_with_state_dir(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        let settings = settings_store::Settings::load_or_init_at(&settings_path);
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings,
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    fn toggle_bracket_pair_colorization_flips_the_real_setting_and_persists_across_reload(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());

        assert!(
            app.read_with(cx, |app, _| app
                .settings
                .appearance
                .bracket_pair_colorization),
            "sanity check: the real default is on - this shipped enabled, so the setting is an \
             opt-out, not an opt-in"
        );

        app.update(cx, |app, cx| {
            app.toggle_bracket_pair_colorization(cx);
        });
        assert!(
            !app.read_with(cx, |app, _| app
                .settings
                .appearance
                .bracket_pair_colorization),
            "the real Settings field must have flipped off"
        );
        cx.run_until_parked();

        let (reloaded, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        assert!(
            !reloaded.read_with(cx, |app, _| app
                .settings
                .appearance
                .bracket_pair_colorization),
            "the toggle must have really been persisted to disk, not just flipped in memory"
        );

        app.update(cx, |app, cx| {
            app.toggle_bracket_pair_colorization(cx);
        });
        assert!(
            app.read_with(cx, |app, _| app
                .settings
                .appearance
                .bracket_pair_colorization),
            "the real Settings field must have flipped back on"
        );
    }

    /// The setting is what `AdeApp::highlight_options` reports - i.e. the value really reaches the
    /// highlighting pipeline, rather than being a persisted field nothing consumes.
    #[gpui::test]
    fn the_setting_really_drives_the_highlight_options_the_pipeline_consumes(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);

        assert!(
            app.read_with(cx, |app, _| app
                .highlight_options()
                .bracket_pair_colorization),
            "options must start enabled"
        );
        app.update(cx, |app, cx| {
            app.toggle_bracket_pair_colorization(cx);
        });
        assert!(
            !app.read_with(cx, |app, _| app
                .highlight_options()
                .bracket_pair_colorization),
            "flipping the setting must really change what the highlight pipeline is handed"
        );
    }

    /// The real Settings UI row: it paints, and clicking it flips the real persisted value. This
    /// is what would catch a row wired to nothing, or wired to the wrong handler.
    #[gpui::test]
    fn the_appearance_page_row_paints_and_clicking_it_flips_the_real_setting(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);

        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Appearance, window, cx);
        });
        cx.run_until_parked();

        // This row now sits past the bottom of the page's own scroll viewport (GitHub issue #216
        // added a row above it), so it has to be scrolled to before it can be clicked - exactly
        // what a real user does. Without this the click lands on a clipped position and silently
        // hits nothing, which is a real hazard for a test that would otherwise still "pass" its
        // `debug_bounds` lookup: painted bounds are recorded even for content scrolled out of
        // view.
        app.update(cx, |app, cx| {
            app.settings_content_scroll_handle.scroll_to_bottom();
            cx.notify();
        });
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("settings-bracket-pair-colorization")
            .expect("the Bracket pair colors toggle must really paint on the Appearance page");
        assert!(
            app.read_with(cx, |app, _| app
                .settings
                .appearance
                .bracket_pair_colorization),
            "premise: on before the click"
        );

        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app
                .settings
                .appearance
                .bracket_pair_colorization),
            "clicking the real row must flip the real setting"
        );

        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app
                .settings
                .appearance
                .bracket_pair_colorization),
            "and clicking it again must flip it back"
        );
    }
}

/// GitHub issue #216's Appearance rows, driven through the same real mutators a click invokes.
///
/// Scoped to the platforms that render the rows at all - see
/// [`AdeApp::render_display_scale_override_rows`]'s `#[cfg]` pair.
///
/// The honest limit of this coverage: it proves the setting is a real, persisted, correctly
/// clamped tri-state that survives a reload, and `crate::x11_scale_factor_env_tests` proves which
/// string that turns into for `GPUI_X11_SCALE_FACTOR`. Neither proves GPUI then paints at that
/// scale - that happens inside a pinned dependency during X11 client init, against a real display
/// this test harness does not have.
#[cfg(test)]
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod display_scale_override_settings_tests {
    use crate::root::AdeApp;
    use crate::settings::state::SettingsPage;
    use crate::settings::store as settings_store;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    /// Same real-load-before-construct helper the neighbouring settings tests use.
    fn open_app_with_state_dir(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        let settings = settings_store::Settings::load_or_init_at(&settings_path);
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings,
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    fn the_toggle_turns_detection_into_a_real_forced_factor_and_persists_it(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            None,
            "sanity check: an install that never touched this keeps GPUI's own detection"
        );

        app.update(cx, |app, cx| app.toggle_display_scale_override(cx));
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            Some(settings_store::DISPLAY_SCALE_OVERRIDE_DEFAULT),
            "turning it on must produce a real unscaled 1.0, which is the reported bug's own fix"
        );
        cx.run_until_parked();

        let (reloaded, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());
        assert_eq!(
            reloaded.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            Some(settings_store::DISPLAY_SCALE_OVERRIDE_DEFAULT),
            "it must have really reached disk - `main` reads the file, not this process's memory"
        );

        app.update(cx, |app, cx| app.toggle_display_scale_override(cx));
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            None,
            "turning it back off must restore detection, not leave a forced 1.0 behind"
        );
        cx.run_until_parked();

        let (reloaded_off, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        assert_eq!(
            reloaded_off.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            None
        );
    }

    #[gpui::test]
    fn the_stepper_moves_in_whole_steps_and_stops_at_the_real_bounds(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        let step = settings_store::DISPLAY_SCALE_OVERRIDE_STEP;

        app.update(cx, |app, cx| {
            // While the override is off there is nothing to step, and stepping must not switch it
            // on behind a control the page isn't showing.
            app.adjust_display_scale_override(step, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            None
        );

        app.update(cx, |app, cx| {
            app.toggle_display_scale_override(cx);
            app.adjust_display_scale_override(step, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            Some(1.05)
        );

        // Twenty more increments must land exactly on 2.05, not on an accumulated float drift -
        // the value is written to `settings.toml` and exported to GPUI as-is.
        app.update(cx, |app, cx| {
            for _ in 0..20 {
                app.adjust_display_scale_override(step, cx);
            }
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            Some(2.05)
        );

        app.update(cx, |app, cx| {
            for _ in 0..200 {
                app.adjust_display_scale_override(step, cx);
            }
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            Some(settings_store::DISPLAY_SCALE_OVERRIDE_MAX),
            "the stepper must clamp to the same bound a hand-edited file is clamped to"
        );

        app.update(cx, |app, cx| {
            for _ in 0..200 {
                app.adjust_display_scale_override(-step, cx);
            }
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            Some(settings_store::DISPLAY_SCALE_OVERRIDE_MIN),
            "and must never reach the zero or negative that would panic GPUI outright"
        );
    }

    /// The real Appearance page row: it paints, and a real click on it flips the real persisted
    /// value - what would catch a row wired to nothing or wired to the wrong handler.
    #[gpui::test]
    fn the_appearance_page_row_paints_and_clicking_it_flips_the_real_setting(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);

        cx.dispatch_action(crate::settings::ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Appearance, window, cx);
        });
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("settings-display-scale-override")
            .expect("the Override display scale toggle must really paint on the Appearance page");

        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            Some(settings_store::DISPLAY_SCALE_OVERRIDE_DEFAULT),
            "clicking the real row must set a real forced factor"
        );

        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.display_scale_override),
            None,
            "and clicking it again must hand detection back to GPUI"
        );
    }
}

/// GitHub issue #226's Notifications page - real paint-and-click coverage for the master switch,
/// one event's own toggle, and picking a different sound from the real popover, plus the same
/// "flip, run_until_parked, reopen against the same file" persistence-across-reload shape
/// `bracket_pair_colorization_settings_tests` already established. `sound::flow`'s own
/// `adeapp_tests` module covers the gating predicate and the transition/seeding logic that don't
/// need any of this page's UI at all.
#[cfg(test)]
mod sound_settings_page_tests {
    use super::*;
    use crate::root::AdeApp;
    use crate::settings::store as settings_store;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    /// Same real-load-before-construct shape as
    /// `bracket_pair_colorization_settings_tests::open_app_with_state_dir` - loading from disk
    /// first (rather than handing a fresh `Settings::default()` alongside a separate path) is
    /// what makes reopening this same helper against the same `settings_path` a genuine "does it
    /// survive a real reload" check.
    fn open_app_with_state_dir(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        let settings = settings_store::Settings::load_or_init_at(&settings_path);
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings,
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    fn open_notifications_page(app: &gpui::Entity<AdeApp>, cx: &mut gpui::VisualTestContext) {
        cx.dispatch_action(crate::settings::ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Notifications, window, cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_master_switch_paints_off_by_default_and_clicking_it_flips_and_persists(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());

        assert!(
            !app.read_with(cx, |app, _| app.settings.sound.enabled),
            "sanity check: sound design is off by default (GitHub issue #226)"
        );
        open_notifications_page(&app, cx);

        let bounds = cx
            .debug_bounds("settings-sound-enabled")
            .expect("the master switch must really paint on the Notifications page");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.settings.sound.enabled),
            "clicking the real row must flip the real setting"
        );

        let (reloaded, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        assert!(
            reloaded.read_with(cx, |app, _| app.settings.sound.enabled),
            "the flip must really be persisted to disk, not just held in memory"
        );
    }

    #[gpui::test]
    fn a_single_event_toggle_paints_and_clicking_it_flips_only_that_event(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        open_notifications_page(&app, cx);

        // The master switch is off by default, which now makes every event row's own toggle
        // non-interactive (see `an_event_toggle_does_nothing_while_the_master_switch_is_off`) -
        // switch it on first so this test still exercises a real, live click on the row itself.
        app.update(cx, |app, cx| app.toggle_sound_enabled(cx));
        cx.run_until_parked();

        assert!(app.read_with(cx, |app, _| app.settings.sound.app_start.enabled));
        let bounds = cx
            .debug_bounds("settings-sound-app-start-enabled")
            .expect("the App start row's own toggle must really paint");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.settings.sound.app_start.enabled),
            "the clicked event's own toggle must have flipped off"
        );
        assert!(
            app.read_with(cx, |app, _| app.settings.sound.agent_finished.enabled),
            "an unrelated event's toggle must be untouched"
        );
    }

    /// Clicking the sound-choice trigger opens the picker, and clicking a row in it both assigns
    /// that sound to the event and closes the popover - the real end-to-end path
    /// `Self::open_sound_picker`/`Self::select_sound_for_event` wire together.
    #[gpui::test]
    fn picking_a_sound_from_the_popover_assigns_it_and_closes_the_popover(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());
        open_notifications_page(&app, cx);
        // Same "the master switch is off by default, and the trigger is now gated on it" reason
        // as `a_single_event_toggle_paints_and_clicking_it_flips_only_that_event`.
        app.update(cx, |app, cx| app.toggle_sound_enabled(cx));
        cx.run_until_parked();

        let default_sound = app.read_with(cx, |app, _| app.settings.sound.app_start.sound.clone());
        let library = app.read_with(cx, |app, _| app.sound_library.clone());
        let other_sound = library
            .iter()
            .find(|sound| sound.id != default_sound)
            .expect("the built-in library has more than one sound")
            .id
            .clone();

        let trigger_bounds = cx
            .debug_bounds("settings-sound-picker-trigger-app_start")
            .expect("the App start row's sound-choice trigger must really paint");
        cx.simulate_click(trigger_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.sound_picker_open.is_some()),
            "clicking the trigger must really open the picker"
        );

        let row_index = library
            .iter()
            .position(|sound| sound.id == other_sound)
            .expect("the target sound must be a real row in the popover");
        // `debug_bounds` wants a `&'static str`; a `Box::leak` is fine in a test - this process
        // exits at the end of the test binary run anyway, and it's the same trick a handful of
        // other dynamically-indexed selectors in this file's own test modules already use.
        let row_selector: &'static str =
            Box::leak(format!("settings-sound-picker-row-{row_index}").into_boxed_str());
        let row_bounds = cx
            .debug_bounds(row_selector)
            .expect("the target sound's own row must really paint in the popover");
        cx.simulate_click(row_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.sound.app_start.sound.clone()),
            other_sound,
            "picking a row must really assign that sound to the event"
        );
        assert!(
            app.read_with(cx, |app, _| app.sound_picker_open.is_none()),
            "picking a row must close the popover"
        );

        let (reloaded, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        assert_eq!(
            reloaded.read_with(cx, |app, _| app.settings.sound.app_start.sound.clone()),
            other_sound,
            "the picked sound must really be persisted to disk"
        );
    }

    /// While the master switch is off (the real default - see the sanity check in
    /// `the_master_switch_paints_off_by_default_and_clicking_it_flips_and_persists`), an event
    /// row's own toggle must be inert: it still paints (so a real test, and a real user, can find
    /// it) but clicking it must not flip the setting, since
    /// `Self::render_toggle_control_gated`'s `interactive: false` path never attaches a click
    /// handler at all.
    #[gpui::test]
    fn an_event_toggle_does_nothing_while_the_master_switch_is_off(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        open_notifications_page(&app, cx);

        assert!(
            !app.read_with(cx, |app, _| app.settings.sound.enabled),
            "sanity check: the master switch is off by default"
        );
        assert!(app.read_with(cx, |app, _| app.settings.sound.agent_finished.enabled));

        let bounds = cx
            .debug_bounds("settings-sound-agent-finished-enabled")
            .expect("the Agent finished row's own toggle must still paint while dimmed");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.settings.sound.agent_finished.enabled),
            "clicking a dimmed event toggle must not flip it - the master switch is off"
        );
    }

    /// Same "no handler attached" guarantee as the toggle test above, for the sound-choice
    /// trigger: clicking it while the master switch is off must not open the picker popover.
    #[gpui::test]
    fn the_sound_choice_trigger_does_nothing_while_the_master_switch_is_off(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        open_notifications_page(&app, cx);

        assert!(!app.read_with(cx, |app, _| app.settings.sound.enabled));

        let trigger_bounds = cx
            .debug_bounds("settings-sound-picker-trigger-app_start")
            .expect("the App start row's sound-choice trigger must still paint while dimmed");
        cx.simulate_click(trigger_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.sound_picker_open.is_none()),
            "clicking a dimmed sound-choice trigger must not open the picker"
        );
    }

    /// Turning the master switch off while an event's sound picker is open must close the
    /// popover - otherwise it would sit open, pointing at a trigger that no longer responds to
    /// clicks (`Self::toggle_sound_enabled`'s own `sound_picker_open = None` on the off path).
    ///
    /// The master switch is flipped by calling `Self::toggle_sound_enabled` directly rather than
    /// simulating a click on its painted bounds: the picker's own full-page scrim
    /// (`Self::render_sound_picker`'s `settings-sound-picker-scrim`) sits on top of the whole
    /// page precisely to catch an outside click and close the popover, so a simulated click
    /// landing on the master switch while the popover is open would hit that scrim first and
    /// never reach the switch at all - not a real way to reach this guard, just an artifact of
    /// coordinates overlapping in a headless test.
    #[gpui::test]
    fn switching_the_master_off_closes_an_open_sound_picker(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let (app, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        open_notifications_page(&app, cx);

        // Switch the master on first, so the trigger is interactive and the picker can really
        // open.
        app.update(cx, |app, cx| app.toggle_sound_enabled(cx));
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.settings.sound.enabled));

        let trigger_bounds = cx
            .debug_bounds("settings-sound-picker-trigger-app_start")
            .expect("the App start row's sound-choice trigger must really paint");
        cx.simulate_click(trigger_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.sound_picker_open.is_some()),
            "sanity check: the picker really opened"
        );

        app.update(cx, |app, cx| app.toggle_sound_enabled(cx));
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.settings.sound.enabled),
            "sanity check: the master switch flipped back off"
        );
        assert!(
            app.read_with(cx, |app, _| app.sound_picker_open.is_none()),
            "switching the master off must close an open sound picker"
        );
    }
}
