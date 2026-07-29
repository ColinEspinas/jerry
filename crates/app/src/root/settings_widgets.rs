//! Two real render helpers shared by every settings page backed by
//! `crate::settings_store::Settings` (General, Appearance & scaling, Themes - see
//! `crate::settings_store::ConfigPage`'s own docs for why only those three), plus the real
//! `toggle`/`stepper`/`choice` row-control widgets `design_handoff_jerry_ade/revision/
//! README.md`'s "Settings rows" section defines that this app actually uses today.
//! `crate::root::settings_render` is the only caller; this module exists separately so the
//! visual "control shape" stays independent of any one page's specific field-mutation logic.
//! (That section also defines a `path` control shape - `value` + `Change…` - but no page in
//! this phase wires a real click handler to a `Change…` action, so it isn't built here; see
//! `crate::root::settings_render`'s General page docs for why `Default environment`, the one
//! row it was designed for, isn't built this phase either.)
//!
//! Every row-control text size in this module - the stepper value, the choice-segment labels,
//! the config banner's path/chip/keys-line/`Open file`/`TOML|JSON` text, and the snippet
//! block's title/body/caption - now routes through `Self::ui_text_size`
//! (`crate::theme::ui_scale`), closing a real audit finding: an earlier pass only scaled each
//! row's own *label*/*hint* (`Self::render_settings_row`) and left every row's *control* fixed,
//! visibly obvious on the Appearance page's own "Interface scale" row, where dragging the
//! control grew its own label while the `90% | 100% | 110% | 125%` segment labels right next to
//! it stayed fixed size.

use super::*;
use crate::settings_store::{CfgFormat, ConfigPage, SnippetLineKind};

impl AdeApp {
    /// Spawns a real `xdg-open` subprocess against the real settings file path - the config
    /// banner's `Open file` button (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change
    /// 3). `xdg-open` (freedesktop.org's `xdg-utils`) is the real, conventional way to hand a
    /// path to the desktop environment's own default handler on Linux - deliberately scoped to
    /// Linux only for now (this sandbox's own real verification surface, and this app's only
    /// currently-supported platform - see `crate::keymap::WindowControlsStyle`'s and
    /// `design_handoff_jerry_ade/revision/CHANGELOG.md`'s "Not design changes" note assigning
    /// full cross-platform work to Revision R11); a real, correct macOS (`open`) / Windows
    /// (`cmd /c start`) equivalent is straightforward to add there once this app actually
    /// builds and ships on those platforms, rather than guessed at now with no way to verify it
    /// in this environment. Uses `Command::status` - blocking, but only the background-executor
    /// thread this closure already runs on, never the GPUI foreground thread - so the real child
    /// is always reaped and never left a zombie for the rest of this process's life. Every other
    /// real subprocess spawn in this codebase reaps what it spawns (`lsp_core::proc`'s
    /// `child.wait()`; `crate::terminal_pane`/`pty_core::PtySession` track and reap the real pty
    /// child via `try_wait`/`wait` - see that type's own docs); nothing here is a deliberate
    /// exception to that. A real failure to launch, or a real non-zero exit status, is logged -
    /// never silently swallowed.
    pub(super) fn open_settings_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = settings_store::settings_toml_path() else {
            log::warn!("cannot open settings file: $HOME is not set");
            return;
        };
        cx.background_executor()
            .spawn(async move {
                match std::process::Command::new("xdg-open").arg(&path).status() {
                    Ok(status) if !status.success() => {
                        log::warn!(
                            "xdg-open exited with {status} while opening {}",
                            path.display()
                        );
                    }
                    Ok(_) => {}
                    Err(err) => {
                        log::warn!("failed to open {} via xdg-open: {err}", path.display());
                    }
                }
            })
            .detach();
    }

    /// The config banner (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 3): a
    /// bordered strip directly under a real page's header showing the real settings file path,
    /// that page's real key list (`crate::settings_store::config_keys_line`), the real
    /// `TOML | JSON` segment, and a real `Open file` button. Only ever called for the three
    /// pages `page` can name - see `crate::settings_store::ConfigPage`'s own docs.
    ///
    /// The displayed path switches with the `TOML | JSON` segment (`~/.config/jerry/
    /// settings.toml` vs. `settings.json`), matching `design_handoff_jerry_ade/revision/
    /// CHANGELOG.md`'s own change-3 spec text verbatim ("a `TOML | JSON` segment (switches the
    /// path and the snippet below)"). But there is no real `settings.json` file to open - the
    /// JSON view is a read-only re-serialization of the same loaded [`Settings`] value (see
    /// `crate::settings_store`'s own "TOML is the real file" docs) - so `Open file` is disabled
    /// (this method's own `Open file` button, right below its [`Self::render_choice_control`]
    /// call for the `TOML | JSON` segment) whenever `JSON` is selected, rather than silently
    /// opening the real `.toml` path next to a displayed `.json` one that button doesn't
    /// actually target.
    pub(super) fn render_config_banner(
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
                |this, index, cx| {
                    // Structural, not a label re-match: index 0 is `TOML`, index 1 is `JSON`,
                    // per the `options` array literal right above - see
                    // `Self::render_choice_control`'s own docs for why dispatch is index-based.
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
                        // No real `settings.json` file exists to open (see this method's own
                        // docs) - a real, visibly disabled affordance, not a clickable button
                        // that would silently open the `.toml` path instead of the `.json` one
                        // the banner is showing right next to it.
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

    /// The snippet block (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 3): "In
    /// settings.toml" (or "In settings.json"), then `page`'s real keys/values pulled from the
    /// actual currently-loaded [`Self::settings`] - see `crate::settings_store::snippet_lines`'s
    /// own docs for why this can never drift from the real file's own contents. Only ever
    /// called for the three pages `page` can name.
    pub(super) fn render_snippet_block(&self, page: ConfigPage) -> impl IntoElement {
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
    pub(super) fn render_settings_row(
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
    /// rows" spec) - `id` must be unique per row (used as the GPUI element id).
    pub(super) fn render_toggle_control(
        &self,
        id: impl Into<gpui::ElementId>,
        on: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .cursor_pointer()
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
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                on_click(this, cx);
            }))
    }

    /// The real `− value +` stepper control (`design_handoff_jerry_ade/revision/README.md`'s
    /// "Settings rows" spec) - `value` is already-formatted display text (e.g. `"13 px"`).
    pub(super) fn render_stepper_control(
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

    /// The real segmented `choice` control (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s
    /// change 3: "a segmented control matching the Diff/File toggle"), and - since this codebase's
    /// own R5.5 senior-maintainer audit - the one real shared implementation behind every
    /// segmented-control-shaped widget in this app: `Self::render_diff_file_toggle`,
    /// `Self::render_right_sidebar_toggle`, `Self::render_palette_scope_control`, and this page's
    /// own TOML/JSON config-format toggle all used to hand-roll the identical visual shape
    /// independently (one of them, `render_diff_file_toggle`, even said so explicitly in its own
    /// doc comment: "mirrors `render_right_sidebar_toggle`'s own segmented-control shape
    /// verbatim") - now all four route through here. `selected` is compared by value (`==`)
    /// against each [`ChoiceOption::label`], so callers pass whatever already-real string they
    /// derive `selected` from.
    ///
    /// Consolidating four independently-evolved implementations into one surfaced a handful of
    /// real, tiny visual disagreements between them that had never been reconciled (some used
    /// `theme::text::DIMMER` for an inactive-but-enabled segment where this control's own
    /// pre-existing shape used the near-identical `theme::settings::SUBTITLE`; the Diff/File
    /// toggle's text didn't respond to `Self::ui_text_size` the way every other segmented control
    /// already did; the TOML/JSON toggle used `theme::font::MONO`/`10.0px`/an `18px` row height
    /// where every other one uses `theme::font::SANS`/`10.5px`/`19px`; and the track had no real
    /// `.gap(px(2.0))` at all, losing the small visible gap between the active pill and its
    /// neighboring segment that three of the four call sites' own pre-consolidation tracks had) -
    /// resolved by keeping this control's own pre-existing, already-in-production (two real
    /// Settings pages) shape as the one real source of truth, rather than picking a shape per
    /// call site.
    ///
    /// `on_select` receives the clicked segment's real **index** into `options`, not its display
    /// `label` - every one of the four real call sites (`Self::render_diff_file_toggle`,
    /// `Self::render_right_sidebar_toggle`, `Self::render_palette_scope_control`, this page's own
    /// TOML/JSON toggle) needs to turn a click back into one of its own real enum variants, and
    /// dispatching that by re-matching on the display string used to be a real, silent
    /// correctness hazard: renaming a segment's label (e.g. changing what `PaletteScope::Files`'s
    /// `label()` returns) with no matching update to the `on_select` match arm would silently
    /// dispatch to the wrong variant - no compile error, no test failure unless someone thought
    /// to check it. An index is structural, not display text, so a label rename alone can't
    /// change what a click dispatches - only actually reordering `options` (a far rarer, more
    /// visible edit, and not the bug class this closes) can.
    pub(super) fn render_choice_control(
        &self,
        id_prefix: &'static str,
        options: &[ChoiceOption],
        selected: String,
        cx: &mut Context<Self>,
        on_select: impl Fn(&mut Self, usize, &mut Context<Self>) + Clone + 'static,
    ) -> impl IntoElement {
        let mut track = div()
            .flex_none()
            .flex()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(theme::radius::BUTTON)
            .bg(theme::surface::SEGMENT_TRACK);

        for (index, option) in options.iter().enumerate() {
            let label = option.label;
            let is_active = label == selected;
            let on_select = on_select.clone();
            let mut segment = div()
                .id(gpui::ElementId::from(format!("{id_prefix}-{label}")))
                // A real, structural (index-based, not label-text-based) lookup key for
                // `gpui::VisualTestContext::debug_bounds` - lets a real interaction test click
                // "the segment at this position" without depending on its current display text,
                // the same real position-not-text distinction [`Self::render_choice_control`]'s
                // own `on_select` dispatch now makes. A no-op in a real production build (see
                // `InteractiveElement::debug_selector`'s own docs).
                .debug_selector(move || format!("choice-{id_prefix}-{index}"))
                .h(px(19.0))
                .px(px(9.0))
                .rounded(theme::radius::CHIP)
                .flex()
                .items_center()
                .gap(px(6.0))
                .when(is_active, |el| el.bg(theme::surface::SEGMENT_ACTIVE))
                .child(
                    div()
                        .font(font(theme::font::SANS))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(self.ui_text_size(10.5))
                        .text_color(if is_active {
                            theme::text::PRIMARY
                        } else if option.enabled {
                            theme::settings::SUBTITLE
                        } else {
                            theme::text::DISABLED
                        })
                        .child(label),
                )
                .when_some(option.hint, |el, hint| {
                    // A real, horizontal sibling of the label (a flex row with a small gap),
                    // not text stacked underneath it - matching the original palette scope
                    // control's own real layout, the only pre-consolidation call site that used
                    // this hint slot.
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
                    move |this, _event: &ClickEvent, _window, cx| {
                        on_select(this, index, cx);
                    },
                ));
            }
            track = track.child(segment);
        }
        track
    }
}

/// One real segment of [`AdeApp::render_choice_control`]. The common case is just a label
/// ([`Self::new`]) - real disabled state and/or a secondary hint rendered as a horizontal
/// sibling of the label (not stacked underneath it) are opt-in for the two real call sites
/// that need them (the Diff/File toggle's `Diff` segment when there's no diff to show; the
/// palette scope control's per-segment keybinding hint).
#[derive(Clone, Copy)]
pub(super) struct ChoiceOption {
    pub(super) label: &'static str,
    pub(super) enabled: bool,
    pub(super) hint: Option<&'static str>,
}

impl ChoiceOption {
    pub(super) fn new(label: &'static str) -> Self {
        Self {
            label,
            enabled: true,
            hint: None,
        }
    }

    pub(super) fn enabled_if(label: &'static str, enabled: bool) -> Self {
        Self {
            label,
            enabled,
            hint: None,
        }
    }

    pub(super) fn with_hint(label: &'static str, hint: &'static str) -> Self {
        Self {
            label,
            enabled: true,
            hint: Some(hint),
        }
    }
}
