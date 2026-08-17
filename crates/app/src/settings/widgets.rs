//! Two render helpers shared by every settings page backed by `crate::settings::store::Settings`
//! (General, Appearance & scaling, Themes - see `crate::settings::store::ConfigPage`'s own docs
//! for why only those three), plus the `toggle`/`stepper`/`choice` row-control widgets
//! `design_handoff_jerry_ade/revision/README.md`'s "Settings rows" section defines.
//! `crate::settings::render` is the only caller; this module exists separately so the
//! visual "control shape" stays independent of any one page's field-mutation logic. (That
//! section also defines a `path` control shape - `value` + `Change…` - but no page wires a
//! click handler to a `Change…` action, so it isn't built here.)

use super::*;
use crate::settings::store::{CfgFormat, ConfigPage, SnippetLineKind};
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Which program + args hand `target` to `target_os`'s default open handler - `target_os` is an
/// injected `std::env::consts::OS`-shaped string (`"linux"`/`"macos"`/`"windows"`/...) so this
/// pure decision is directly unit-testable without actually spawning a process, same "pure
/// decision function + thin real-execution wrapper" shape as `crate::keymap`'s
/// `detected_platform_is_macos`/`resolve_keystroke` and `crate::env_info`'s `is_wsl_from`.
fn open_command_for(target_os: &str, target: &OsStr) -> (&'static str, Vec<OsString>) {
    match target_os {
        "macos" => ("open", vec![target.to_os_string()]),
        "windows" => (
            "cmd",
            vec![
                "/c".into(),
                "start".into(),
                "".into(),
                target.to_os_string(),
            ],
        ),
        _ => ("xdg-open", vec![target.to_os_string()]),
    }
}

impl AdeApp {
    /// Spawns `program args` on the background executor - never the GPUI foreground thread - and
    /// logs a failed launch or a non-zero exit rather than silently swallowing it. Shared by
    /// every real "open something via the OS default handler" call site
    /// ([`Self::open_settings_file`], [`Self::open_install_url`]) so this real
    /// spawn-and-reap-safely logic lives in exactly one place, not duplicated per call site.
    /// Uses `Command::status` - blocking, but only on the background-executor thread - so the
    /// child is always reaped, matching every other subprocess spawn in this codebase
    /// (`lsp_core::proc`'s `child.wait()`; `pty_core::PtySession`'s `try_wait`/`wait`).
    /// `target_label` is only ever used for the log message, never passed to the child process.
    pub(crate) fn spawn_open_command(
        &mut self,
        program: &'static str,
        args: Vec<OsString>,
        target_label: String,
        cx: &mut Context<Self>,
    ) {
        cx.background_executor()
            .spawn(async move {
                match std::process::Command::new(program).args(&args).status() {
                    Ok(status) if !status.success() => {
                        log::warn!("{program} exited with {status} while opening {target_label}");
                    }
                    Ok(_) => {}
                    Err(err) => {
                        log::warn!("failed to open {target_label} via {program}: {err}");
                    }
                }
            })
            .detach();
    }

    /// Hands a real local path to this platform's default-open handler - the file tree's
    /// "Reveal in file manager" row (`crate::sidebar::tree_ops::AdeApp::reveal_in_file_manager`).
    /// The same [`open_command_for`]/[`Self::spawn_open_command`] pair
    /// [`Self::open_settings_file`] uses, exposed as its own entry point so the sidebar doesn't
    /// have to reach into a settings-page method (or, worse, grow a second `xdg-open` call site).
    pub(crate) fn open_path_with_os_handler(&mut self, path: &Path, cx: &mut Context<Self>) {
        let (program, args) = open_command_for(std::env::consts::OS, path.as_os_str());
        self.spawn_open_command(program, args, path.display().to_string(), cx);
    }

    /// Opens the settings file - the config banner's `Open file` button - via
    /// [`open_command_for`], real per-platform: `xdg-open` on Linux, `open` on macOS,
    /// `cmd /c start` on Windows (see that function's docs for why each is correct).
    pub(in crate::settings) fn open_settings_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = settings_store::settings_toml_path() else {
            log::warn!(
                "cannot open settings file: no home directory ($HOME, or %USERPROFILE% on Windows)"
            );
            return;
        };
        let (program, args) = open_command_for(std::env::consts::OS, path.as_os_str());
        self.spawn_open_command(program, args, path.display().to_string(), cx);
    }

    /// Opens a language server's real install/docs `url` (`crate::language::SettingsLspRow::
    /// install_url`) in the user's default browser - the Language servers settings page's
    /// `Install` action on a genuinely `not installed` row (see
    /// `crate::settings::state::LspRow::is_ready`). Reuses [`open_command_for`]/
    /// [`Self::spawn_open_command`] exactly as [`Self::open_settings_file`] does - this is the
    /// same real "hand a target to the OS default-open handler" mechanism pointed at a URL
    /// instead of a local file path, not a second, independent implementation of it. This app
    /// never runs an install command on the user's behalf; the user follows the real, official
    /// instructions on the page this opens.
    pub(in crate::settings) fn open_install_url(
        &mut self,
        url: &'static str,
        cx: &mut Context<Self>,
    ) {
        let (program, args) = open_command_for(std::env::consts::OS, OsStr::new(url));
        self.spawn_open_command(program, args, url.to_string(), cx);
    }

    /// The config banner (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 3): a
    /// bordered strip directly under a real page's header showing the real settings file path,
    /// that page's real key list (`crate::settings::store::config_keys_line`), the real
    /// `TOML | JSON` segment, and an `Open file` button. Only ever called for the three pages
    /// `page` can name - see `crate::settings::store::ConfigPage`'s own docs.
    pub(in crate::settings) fn render_config_banner(
        &self,
        page: ConfigPage,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let toml_path = settings_store::settings_toml_path();
        let json_path = settings_store::settings_json_display_path();
        let is_json = self.settings_cfg_format == CfgFormat::Json;
        let display_path = match self.settings_cfg_format {
            CfgFormat::Toml => toml_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.config/jerry/settings.toml".to_string()),
            CfgFormat::Json => json_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.config/jerry/settings.json".to_string()),
        };
        let keys_line = settings_store::config_keys_line(page);

        div()
            .mt(px(14.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(9.0))
            .px(px(10.0))
            .py(px(7.0))
            .rounded(theme::radius::CARD)
            .border_1()
            .border_color(theme::border::CARD)
            .bg(theme::surface::CARD)
            .child(
                div()
                    .flex_none()
                    .w(px(15.0))
                    .h(px(15.0))
                    .rounded(theme::radius::BUTTON)
                    .bg(theme::surface::CHIP_NEUTRAL)
                    .flex()
                    .items_center()
                    .justify_center()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(7.0))
                    .text_color(theme::text::DIM)
                    .child("to"),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::SECONDARY)
                    .child(display_path),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::GHOSTER)
                    .child(keys_line),
            )
            .child(self.render_choice_control(
                "settings-cfg-fmt",
                &[ChoiceOption::new("TOML"), ChoiceOption::new("JSON")],
                self.settings_cfg_format.label().to_string(),
                cx,
                |this, index, _window, cx| {
                    // Index into the `options` array above, not a label re-match.
                    this.settings_cfg_format = match index {
                        1 => CfgFormat::Json,
                        _ => CfgFormat::Toml,
                    };
                    cx.notify();
                },
            ))
            .child(
                div()
                    .id("settings-open-file")
                    .flex_none()
                    .h(px(20.0))
                    .px(px(8.0))
                    .rounded(theme::radius::BUTTON)
                    .border_1()
                    .flex()
                    .items_center()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .child("Open file")
                    .when(is_json, |el| {
                        // No real settings.json file exists to open - see this method's docs.
                        el.cursor_default()
                            .border_color(theme::border::BUTTON_DISABLED)
                            .text_color(theme::text::GHOSTER)
                    })
                    .when(!is_json, |el| {
                        el.cursor_pointer()
                            .border_color(theme::border::BUTTON)
                            .text_color(theme::text::MUTED)
                            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.open_settings_file(cx);
                            }))
                    }),
            )
    }

    /// The snippet block (`CHANGELOG.md`'s change 3): "In settings.toml" (or "In settings.json"),
    /// then `page`'s keys/values pulled from the currently-loaded [`Self::settings`] - see
    /// `crate::settings::store::snippet_lines`'s docs for why this can't drift from the file's
    /// own contents. Only ever called for the three pages `page` can name.
    pub(in crate::settings) fn render_snippet_block(&self, page: ConfigPage) -> impl IntoElement {
        let lines = settings_store::snippet_lines(&self.settings, page, self.settings_cfg_format);
        let title = format!(
            "In {}",
            match self.settings_cfg_format {
                CfgFormat::Toml => "settings.toml",
                CfgFormat::Json => "settings.json",
            }
        );

        div()
            .py(px(20.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .pb(px(6.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child(title),
            )
            .child(
                div()
                    .rounded(theme::radius::CARD)
                    .border_1()
                    .border_color(theme::border::CARD)
                    .bg(theme::surface::FOOTER)
                    .px(px(12.0))
                    .py(px(9.0))
                    .flex()
                    .flex_col()
                    .children(lines.into_iter().map(|line| {
                        let color = match line.kind {
                            SnippetLineKind::Section => theme::settings::SNIPPET_SECTION,
                            SnippetLineKind::Key => theme::text::SECONDARY,
                        };
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(11.0))
                            .line_height(px(18.0))
                            .text_color(color)
                            .child(if line.text.is_empty() {
                                " ".to_string()
                            } else {
                                line.text
                            })
                    })),
            )
            .child(
                div()
                    .mt(px(7.0))
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(11.0))
                    .line_height(px(16.0))
                    .text_color(theme::text::FAINTER)
                    .child(
                        "The file is the source of truth - this panel is a real, live view of it. \
                         Hand edits are picked up the next time Jerry starts.",
                    ),
            )
    }

    /// One real settings row shell - `design_handoff_jerry_ade/revision/README.md`'s "Settings
    /// rows" spec: "11px vertical padding, bottom border, label + hint on the left, control
    /// right." `control` is whichever of [`Self::render_toggle_control`]/
    /// [`Self::render_stepper_control`]/[`Self::render_choice_control`] the caller built.
    pub(in crate::settings) fn render_settings_row(
        &self,
        label: &'static str,
        hint: &'static str,
        control: impl IntoElement,
    ) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(16.0))
            .py(px(11.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(self.ui_text_size(12.0))
                            .text_color(theme::text::HEADING)
                            .child(label),
                    )
                    .when(!hint.is_empty(), |el| {
                        el.child(
                            div()
                                .mt(px(2.0))
                                .font(font(theme::font::SANS))
                                .text_size(self.ui_text_size(11.0))
                                .line_height(px(16.0))
                                .text_color(theme::text::FAINT)
                                .child(hint),
                        )
                    }),
            )
            .child(control)
    }

    /// The real 26×15 toggle control (`design_handoff_jerry_ade/revision/README.md`'s "Settings
    /// rows" spec) - `id` must be unique per row (used as the GPUI element id). Always
    /// interactive - see [`Self::render_toggle_control_gated`] for the variant a parent
    /// switch/setting can disable.
    pub(in crate::settings) fn render_toggle_control(
        &self,
        id: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        self.render_toggle_control_gated(id, on, true, cx, on_click)
    }

    /// [`Self::render_toggle_control`], but the click handler and pointer cursor are only
    /// attached while `interactive` is `true` - the same "just don't attach the handler" shape
    /// [`ChoiceOption::enabled_if`]'s own rendering below uses for a disabled option, rather than
    /// a separate visual "disabled" track/knob pair. Callers pair this with an opacity wrapper on
    /// the whole row for the dimmed look (see
    /// `crate::settings::render::AdeApp::render_sound_event_row`); this method itself only ever
    /// decides interactivity. `debug_selector` is kept even when `!interactive`, so a test can
    /// still find the element and assert that clicking it does nothing.
    pub(in crate::settings) fn render_toggle_control_gated(
        &self,
        id: &'static str,
        on: bool,
        interactive: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            // Same lookup key as the element id, so a real test can measure this toggle's own
            // painted bounds and click it (`VisualTestContext::debug_bounds`) rather than
            // asserting only on the field the handler happens to write - see
            // `crate::settings::render::bracket_pair_colorization_settings_tests`. A no-op
            // outside test builds, matching every other `debug_selector` in this crate; narrowed
            // from `impl Into<ElementId>` to `&'static str` to make it available, which every one
            // of this method's call sites already passed.
            .debug_selector(move || id.to_string())
            .flex_none()
            .w(px(26.0))
            .h(px(15.0))
            .rounded(theme::radius::PILL)
            .px(px(2.0))
            .flex()
            .items_center()
            .when(on, |el| el.justify_end())
            .when(!on, |el| el.justify_start())
            .bg(if on {
                theme::toggle::TRACK_ON
            } else {
                theme::toggle::TRACK_OFF
            })
            .child(div().w(px(11.0)).h(px(11.0)).rounded(px(5.5)).bg(if on {
                theme::toggle::KNOB_ON
            } else {
                theme::toggle::KNOB_OFF
            }))
            .when(interactive, |el| {
                el.cursor_pointer().on_click(cx.listener(
                    move |this, _event: &ClickEvent, _window, cx| {
                        on_click(this, cx);
                    },
                ))
            })
    }

    /// The real `− value +` stepper control (`design_handoff_jerry_ade/revision/README.md`'s
    /// "Settings rows" spec) - `value` is already-formatted display text (e.g. `"13 px"`).
    pub(in crate::settings) fn render_stepper_control(
        &self,
        id_prefix: &'static str,
        value: String,
        cx: &mut Context<Self>,
        on_dec: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        on_inc: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        type StepClick = Box<dyn Fn(&mut AdeApp, &mut Context<AdeApp>)>;
        let step_button =
            |label: &'static str, id: gpui::ElementId, cx: &mut Context<Self>, click: StepClick| {
                div()
                    .id(id)
                    .cursor_pointer()
                    .w(px(19.0))
                    .h(px(19.0))
                    .rounded(theme::radius::CHIP)
                    .border_1()
                    .border_color(theme::border::BUTTON)
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(11.0))
                    .text_color(theme::text::DIM)
                    .child(label)
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        click(this, cx);
                    }))
            };

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(step_button(
                "\u{2212}",
                gpui::ElementId::from(format!("{id_prefix}-dec")),
                cx,
                Box::new(on_dec),
            ))
            .child(
                div()
                    .min_w(px(46.0))
                    .text_center()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(11.5))
                    .text_color(theme::text::HEADING)
                    .child(value),
            )
            .child(step_button(
                "+",
                gpui::ElementId::from(format!("{id_prefix}-inc")),
                cx,
                Box::new(on_inc),
            ))
    }

    /// The segmented `choice` control (`CHANGELOG.md`'s change 3: "a segmented control matching
    /// the Diff/File toggle") - the one shared implementation behind every segmented-control
    /// widget in this app (`Self::render_diff_file_toggle`, `Self::render_right_sidebar_toggle`,
    /// `Self::render_palette_scope_control`, and this page's TOML/JSON toggle). `selected` is
    /// compared by value (`==`) against each [`ChoiceOption::label`].
    pub(crate) fn render_choice_control(
        &self,
        id_prefix: &'static str,
        options: &[ChoiceOption],
        selected: String,
        cx: &mut Context<Self>,
        on_select: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + Clone + 'static,
    ) -> impl IntoElement {
        let mut track = div()
            .flex_none()
            .flex()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(theme::radius::BUTTON)
            .bg(theme::surface::SEGMENT_TRACK);

        // One `IconRow` for the whole control, not one per segment: that is `REVISION-2026-08-14.md`
        // §7 rule 7 ("a row of icons needs one shared optical box, not one size per icon") made
        // structural rather than left to each segment to get right - the exact defect §4w records
        // fixing after the first cut drew "folder 12x8, magnifier 8x8, diff 7x7".
        let icons =
            crate::icons::IconRow::new(&self.settings.icon_pack, crate::icons::IconSize::PanelTab);
        for (index, option) in options.iter().enumerate() {
            let label = option.label;
            let is_active = label == selected;
            let on_select = on_select.clone();
            let foreground = if is_active {
                theme::text::PRIMARY
            } else if option.enabled {
                theme::settings::SUBTITLE
            } else {
                theme::text::DISABLED
            };
            let mut segment = div()
                .id(gpui::ElementId::from(format!("{id_prefix}-{label}")))
                // Index-based lookup key for `VisualTestContext::debug_bounds`, matching this
                // control's own index-based `on_select` dispatch. No-op in release builds.
                .debug_selector(move || format!("choice-{id_prefix}-{index}"))
                .h(px(19.0))
                // An icon segment is a fixed 26 wide with its glyph centred (§4w's "26x19
                // buttons"); a label segment stays intrinsically sized with its own side padding.
                .map(|el| match option.icon {
                    Some(_) => el.w(px(26.0)).justify_center(),
                    None => el.px(px(9.0)),
                })
                .rounded(theme::radius::CHIP)
                .flex()
                .items_center()
                .gap(px(6.0))
                .when(is_active, |el| el.bg(theme::surface::SEGMENT_ACTIVE))
                .when_some(option.tooltip, |el, tooltip| {
                    el.tooltip(crate::root::widgets::text_tooltip(tooltip))
                })
                .map(|el| match option.icon {
                    // The glyph replaces the label entirely - drawing both would put the label
                    // back on a row §4w emptied on purpose.
                    Some(icon) => el.child(icons.draw(icon, foreground)),
                    None => el.child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(10.5))
                            .text_color(foreground)
                            .child(label),
                    ),
                })
                .when_some(option.hint, |el, hint| {
                    // A horizontal sibling of the label, not text stacked underneath it.
                    el.child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(if is_active {
                                theme::text::DIMMER
                            } else {
                                theme::text::GHOSTER
                            })
                            .child(hint),
                    )
                });
            if option.enabled {
                segment = segment.cursor_pointer().on_click(cx.listener(
                    move |this, _event: &ClickEvent, window, cx| {
                        on_select(this, index, window, cx);
                    },
                ));
                // GitHub issue #128: every other clickable row in the app gives hover feedback -
                // this shared widget (behind the Diff/File toggle, right-sidebar toggle, palette
                // scope control, and the settings TOML/JSON toggle) didn't. Only the *inactive*
                // segments need it - the active one already reads as selected via
                // `SEGMENT_ACTIVE` above, and layering a second bg on top of that would just
                // muddy it.
                if !is_active {
                    segment = segment.hover(|el| el.bg(theme::surface::ROW_HOVER_ALT));
                }
            }
            track = track.child(segment);
        }
        track
    }
}

/// One segment of [`AdeApp::render_choice_control`]. The common case is just a label
/// ([`Self::new`]) - disabled state, a secondary hint (a horizontal sibling of the label), and an
/// icon in place of the label are opt-in for the call sites that need them (the Diff/File toggle's
/// `Diff` segment when there's no diff to show; the palette scope control's per-segment keybinding
/// hint; the right panel's three icon tabs).
#[derive(Clone, Copy)]
pub(crate) struct ChoiceOption {
    /// This segment's identity as well as its default rendering: [`AdeApp::render_choice_control`]
    /// matches `selected` against it. An icon segment still carries one, so selection is never
    /// keyed on something as fragile as which glyph is drawn.
    pub(in crate::settings) label: &'static str,
    pub(in crate::settings) enabled: bool,
    pub(in crate::settings) hint: Option<&'static str>,
    /// Drawn **instead of** the label. `STAGE-A-CHANGELOG.md` §4w: the right panel's `Files` /
    /// `Search` / `Changes` tabs become "three 26x19 buttons in the same segmented shell ... all
    /// three drawn inside one 11x10 optical box", with the labels moving to the tooltips below.
    pub(in crate::settings) icon: Option<crate::icons::Icon>,
    /// The full `"<label> - <hint>"` sentence a segment shows on hover. Mandatory in practice for
    /// an icon segment, which has no visible label left to read - §4w's own "labels move to
    /// tooltips with their hints intact".
    pub(in crate::settings) tooltip: Option<&'static str>,
}

impl ChoiceOption {
    pub(crate) fn new(label: &'static str) -> Self {
        Self {
            label,
            enabled: true,
            hint: None,
            icon: None,
            tooltip: None,
        }
    }

    pub(crate) fn enabled_if(label: &'static str, enabled: bool) -> Self {
        Self {
            label,
            enabled,
            hint: None,
            icon: None,
            tooltip: None,
        }
    }

    pub(crate) fn with_hint(label: &'static str, hint: &'static str) -> Self {
        Self {
            label,
            enabled: true,
            hint: Some(hint),
            icon: None,
            tooltip: None,
        }
    }

    /// An icon segment: the glyph replaces the label on screen, and the label plus its hint move
    /// into `tooltip` verbatim.
    pub(crate) fn with_icon(
        label: &'static str,
        icon: crate::icons::Icon,
        tooltip: &'static str,
    ) -> Self {
        Self {
            label,
            enabled: true,
            hint: None,
            icon: Some(icon),
            tooltip: Some(tooltip),
        }
    }
}

#[cfg(test)]
mod open_command_tests {
    use super::open_command_for;
    use std::ffi::{OsStr, OsString};
    use std::path::Path;

    fn os_strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    /// Every platform's real command, for both target shapes this is genuinely reused for: a
    /// local settings path, and the Language servers page's `https://` install URL
    /// (`AdeApp::open_install_url`). An unrecognized OS falls back to `xdg-open` rather than
    /// failing.
    ///
    /// Windows' empty-string argument is load-bearing, not incidental - see
    /// [`open_command_for`]'s own docs: without it, `start` treats the target itself as its
    /// window-title argument.
    #[test]
    fn every_platform_opens_every_real_target_with_its_own_command() {
        const URL: &str = "https://rust-analyzer.github.io/book/rust_analyzer_binary.html";
        for target in [
            Path::new("/home/x/settings.toml").as_os_str(),
            Path::new(r"C:\Users\x\settings.toml").as_os_str(),
            OsStr::new(URL),
        ] {
            let raw = target.to_string_lossy().into_owned();
            for (os, program, args) in [
                ("macos", "open", os_strings(&[&raw])),
                ("windows", "cmd", os_strings(&["/c", "start", "", &raw])),
                ("linux", "xdg-open", os_strings(&[&raw])),
                ("freebsd", "xdg-open", os_strings(&[&raw])),
            ] {
                assert_eq!(
                    open_command_for(os, target),
                    (program, args),
                    "{os} / {raw}"
                );
            }
        }
    }
}
