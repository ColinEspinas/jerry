//! Surface C's editor zoom: the clamped, persisted zoom percentage, the rem-scoped
//! subtree it applies through, and the zoom control in the surface's own footer.

use super::*;
#[cfg(test)]
use crate::code_surface::fixtures::temp_repo;
use crate::root::rem_scope::WithRemSize;
#[cfg(test)]
use crate::test_support::open_test_app;

impl AdeApp {
    /// Editor-zoom range (70-200%, in steps of 10) and default (100%) - re-exported from
    /// `settings_store`'s real, single source of truth (see that module's "Editor zoom is one
    /// global, persisted number now" docs) so this file's own call sites/doc comments below
    /// didn't need to be renamed.
    pub(in crate::code_surface) const ZOOM_MIN_PERCENT: u16 =
        settings_store::EDITOR_ZOOM_PERCENT_MIN;
    pub(in crate::code_surface) const ZOOM_MAX_PERCENT: u16 =
        settings_store::EDITOR_ZOOM_PERCENT_MAX;
    pub(in crate::code_surface) const ZOOM_STEP_PERCENT: u16 =
        settings_store::EDITOR_ZOOM_PERCENT_STEP;
    pub(crate) const ZOOM_DEFAULT_PERCENT: u16 = settings_store::EDITOR_ZOOM_PERCENT_DEFAULT;

    /// The effective rem size (px) the zoom-scoped code surface renders `rems()`-based text at:
    /// `editor_font_size` times [`Settings.appearance.editor_zoom_percent`]
    /// (`crate::settings::store::AppearanceSettings::editor_zoom_percent`) as a fraction. Both
    /// factors are real, globally-persisted `Settings` fields now - see that field's own docs for
    /// why this used to also read a third, in-memory-only, per-worktree-reset value.
    pub(crate) fn effective_code_rem_px(&self) -> f32 {
        self.settings.appearance.editor_font_size
            * (self.settings.appearance.editor_zoom_percent as f32 / 100.0)
    }

    /// Zooms in one step (the toolbar `+` button).
    pub(crate) fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.set_code_zoom(
            clamp_zoom_percent(
                self.settings.appearance.editor_zoom_percent as i32
                    + Self::ZOOM_STEP_PERCENT as i32,
            ),
            cx,
        );
    }

    /// Zooms out one step (the toolbar `−` button).
    pub(crate) fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.set_code_zoom(
            clamp_zoom_percent(
                self.settings.appearance.editor_zoom_percent as i32
                    - Self::ZOOM_STEP_PERCENT as i32,
            ),
            cx,
        );
    }

    /// Resets zoom to 100% (clicking the toolbar's zoom value).
    pub(crate) fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.set_code_zoom(Self::ZOOM_DEFAULT_PERCENT, cx);
    }

    /// The single place `Settings.appearance.editor_zoom_percent` is written by a user action;
    /// `zoom_in`/`zoom_out`/`reset_zoom` all delegate here. Global and persisted (see
    /// `settings_store`'s module docs) - applies uniformly to every open file, in every worktree,
    /// and queues a real settings-file save via [`Self::persist_settings`], the same writer every
    /// other Settings-page mutation already goes through, so a zoom change made from the toolbar
    /// survives a restart exactly like one made from the Settings page would.
    fn set_code_zoom(&mut self, percent: u16, cx: &mut Context<Self>) {
        self.settings.appearance.editor_zoom_percent = percent;
        self.persist_settings(cx);
        cx.notify();
    }

    /// The toolbar's zoom control group: `-` / value / `+`, 19x19 buttons with a 1px gap, value
    /// in a fixed 36px column (every value in `ZOOM_MIN_PERCENT..=ZOOM_MAX_PERCENT` is at most 3
    /// digits). Clicking the value resets zoom to 100%.
    pub(in crate::code_surface) fn render_zoom_control(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let button = |id: &'static str, label: &'static str, enabled: bool| {
            let mut el = div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .w(px(19.0))
                .h(px(19.0))
                .rounded(theme::radius::CHIP)
                .font(font(theme::font::MONO))
                .text_size(px(11.0))
                .text_color(if enabled {
                    theme::text::DIM
                } else {
                    theme::text::DISABLED
                })
                .child(label);
            if enabled {
                el = el
                    .cursor_pointer()
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT));
            }
            el
        };

        let can_zoom_out = self.settings.appearance.editor_zoom_percent > AdeApp::ZOOM_MIN_PERCENT;
        let can_zoom_in = self.settings.appearance.editor_zoom_percent < AdeApp::ZOOM_MAX_PERCENT;

        div()
            .id("code-zoom-control")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(1.0))
            .child(
                button("code-zoom-out", "\u{2212}", can_zoom_out).when(can_zoom_out, |el| {
                    el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.zoom_out(cx);
                    }))
                }),
            )
            .child(
                div()
                    .id("code-zoom-value")
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(36.0))
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(10.0))
                    .text_color(theme::text::DIM)
                    .hover(|el| el.text_color(theme::text::SELECTED))
                    .child(format!("{}%", self.settings.appearance.editor_zoom_percent))
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.reset_zoom(cx);
                    })),
            )
            .child(
                button("code-zoom-in", "+", can_zoom_in).when(can_zoom_in, |el| {
                    el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.zoom_in(cx);
                    }))
                }),
            )
    }
}

/// Rounds `percent` to the nearest 10-point step, then clamps into
/// `AdeApp::ZOOM_MIN_PERCENT..=AdeApp::ZOOM_MAX_PERCENT`. A free function, not inlined into
/// `zoom_in`/`zoom_out`, so it's unit-testable without a `Context<AdeApp>`. Takes `i32`, not
/// `u16`, so an already out-of-range or negative candidate (e.g. stepping below zero from 70%)
/// doesn't underflow before it's clamped.
pub(in crate::code_surface) fn clamp_zoom_percent(percent: i32) -> u16 {
    let step = AdeApp::ZOOM_STEP_PERCENT as i32;
    let stepped = (percent as f32 / step as f32).round() as i32 * step;
    stepped.clamp(
        AdeApp::ZOOM_MIN_PERCENT as i32,
        AdeApp::ZOOM_MAX_PERCENT as i32,
    ) as u16
}

/// Wraps `content` in [`rem_scope::WithRemSize`], scoped to `rem_px`. Rows using
/// `.text_size(rems(1.0))`/`.line_height(rems(1.6))` scale with it; anything still in `px()`
/// (the line-number gutter, the git-gutter column) is unaffected - covered by
/// `code_zoom_tests::zoom_scales_text_but_not_the_gutter_width`. `pub(crate)` so
/// `crate::merge::render` (Surface D's own conflict view) can reuse the same
/// zoom mechanism for the merge surface's conflict columns, rather than a second one.
pub(crate) fn zoom_scoped(rem_px: f32, content: impl IntoElement) -> gpui::AnyElement {
    WithRemSize::new(px(rem_px))
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(content)
        .into_any_element()
}

/// Coverage for the editor-zoom feature: clamping/rounding logic, zoom-state mutation through
/// [`AdeApp`], both per-tab-zoom modes, and an interaction test proving the scoped
/// `rem_scope::WithRemSize` mechanism scales code text while leaving the fixed-`px()` gutter
/// untouched.
#[cfg(test)]
mod code_zoom_tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn clamp_zoom_percent_stays_put_at_the_documented_boundaries() {
        assert_eq!(clamp_zoom_percent(70), 70);
        assert_eq!(clamp_zoom_percent(200), 200);
        assert_eq!(clamp_zoom_percent(100), 100);
    }

    #[test]
    fn clamp_zoom_percent_clamps_out_of_range_candidates_into_bounds() {
        assert_eq!(
            clamp_zoom_percent(-40),
            70,
            "a negative candidate must clamp to the real minimum, not underflow/wrap"
        );
        assert_eq!(clamp_zoom_percent(5000), 200);
        assert_eq!(clamp_zoom_percent(0), 70);
    }

    #[test]
    fn clamp_zoom_percent_rounds_to_the_nearest_real_ten_point_step() {
        assert_eq!(clamp_zoom_percent(53), 70);
        assert_eq!(clamp_zoom_percent(75), 80);
        assert_eq!(clamp_zoom_percent(84), 80);
        assert_eq!(clamp_zoom_percent(205), 200);
    }

    fn write_single_file(repo: &std::path::Path) -> PathBuf {
        let file_path = repo.join("main.rs");
        std::fs::write(&file_path, "fn main() {\n    let x = 1;\n}\n").expect("write main.rs");
        file_path
    }

    /// A valid `.rs` file of exactly `lines` lines (`// line N` comments) - used by
    /// `zoom_scales_text_but_not_the_gutter_width` to reach a 4-digit line number, which
    /// `write_single_file`'s 3-line file can't produce.
    fn write_many_line_file(repo: &std::path::Path, lines: usize) -> PathBuf {
        let file_path = repo.join("main.rs");
        let mut content = String::new();
        for line in 1..=lines {
            content.push_str(&format!("// line {line}\n"));
        }
        std::fs::write(&file_path, content).expect("write main.rs");
        file_path
    }

    #[gpui::test]
    fn zoom_in_and_out_clamp_at_the_documented_boundaries_through_the_real_app(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = write_single_file(repo.path());
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            AdeApp::ZOOM_DEFAULT_PERCENT,
            "a freshly opened file starts at the real 100% default"
        );

        app.update(cx, |app, cx| {
            for _ in 0..20 {
                app.zoom_out(cx);
            }
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            AdeApp::ZOOM_MIN_PERCENT,
            "zooming out far past the real minimum must clamp at 70%, never go lower"
        );

        app.update(cx, |app, cx| {
            for _ in 0..30 {
                app.zoom_in(cx);
            }
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            AdeApp::ZOOM_MAX_PERCENT,
            "zooming in far past the real maximum must clamp at 200%, never wrap"
        );
    }

    #[gpui::test]
    fn resetting_zoom_returns_to_100_percent(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = write_single_file(repo.path());
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path, window, cx);
        });

        app.update(cx, |app, cx| {
            app.zoom_in(cx);
            app.zoom_in(cx);
            app.zoom_in(cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            130
        );

        app.update(cx, |app, cx| app.reset_zoom(cx));
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            AdeApp::ZOOM_DEFAULT_PERCENT,
            "resetting zoom - the toolbar value's own click affordance - must land exactly on \
             100%, matching design_handoff_jerry_ade/revision/CHANGELOG.md's change 6"
        );
    }

    #[gpui::test]
    fn zoom_applies_globally_to_every_open_file_not_just_the_active_one(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let a = repo.path().join("a.rs");
        let b = repo.path().join("b.rs");
        std::fs::write(&a, "fn a() {}\n").expect("write a.rs");
        std::fs::write(&b, "fn b() {}\n").expect("write b.rs");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        app.update(cx, |app, cx| app.zoom_in(cx)); // 110%, global

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(b.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            110,
            "opening a different file must keep the one global zoom value, not reset to 100%"
        );

        app.update(cx, |app, cx| app.zoom_in(cx)); // 120%, global

        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(PathBuf::from("a.rs"), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            120,
            "switching back to a.rs must show the same global 120% - not the 110% it happened to \
             be at when it was left, which would mean zoom was secretly still tracked per-tab"
        );
    }

    #[gpui::test]
    fn zoom_survives_a_worktree_switch(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let worktree_b = temp_repo();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        // A plain, directly-seeded `worktrees` list (mirroring
        // `lsp_client_eviction_tests::switching_between_several_worktrees_never_lets_lsp_clients_
        // grow_past_one`'s own pattern) - `select_worktree` only needs a real, readable path on
        // disk, not an actual git worktree.
        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                worktrees::WorktreeItem {
                    path: repo.path().to_path_buf(),
                    label: "wt-a".to_string(),
                    branch: None,
                    is_main: true,
                    is_bare: false,
                    is_detached: false,
                    short_sha: None,
                    is_locked: false,
                    lock_reason: None,
                    is_broken: false,
                    broken_reason: None,
                    error: None,
                },
                worktrees::WorktreeItem {
                    path: worktree_b.path().to_path_buf(),
                    label: "wt-b".to_string(),
                    branch: None,
                    is_main: false,
                    is_bare: false,
                    is_detached: false,
                    short_sha: None,
                    is_locked: false,
                    lock_reason: None,
                    is_broken: false,
                    broken_reason: None,
                    error: None,
                },
            ];
        });

        app.update(cx, |app, cx| {
            app.zoom_in(cx);
            app.zoom_in(cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            120
        );

        app.update_in(cx, |app, window, cx| app.select_worktree(1, window, cx));
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            120,
            "zoom is a global Settings field now - a worktree switch must not reset it"
        );
    }

    #[gpui::test]
    fn closing_and_reopening_a_tab_keeps_the_same_global_zoom(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let a = repo.path().join("a.rs");
        std::fs::write(&a, "fn a() {}\n").expect("write a.rs");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.zoom_in(cx);
            app.zoom_in(cx);
            app.zoom_in(cx);
        }); // 130%, global
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            130
        );

        app.update_in(cx, |app, window, cx| {
            app.close_file_tab(PathBuf::from("a.rs"), window, cx);
        });
        assert!(
            !app.read_with(cx, |app, _| app
                .open_files()
                .contains(&PathBuf::from("a.rs"))),
            "closing the tab must really remove it from open_files"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            130,
            "reopening a.rs after closing it must still show the same global 130% zoom, not \
             reset to the 100% default - zoom no longer belongs to any one file"
        );
    }

    #[gpui::test]
    fn zoom_scales_text_but_not_the_gutter_width(cx: &mut TestAppContext) {
        let repo = temp_repo();
        // 1200 lines - enough to reach a 4-digit line number (1000), which the second half
        // needs; line 1 (used by the first half) exists regardless of file size.
        let file_path = write_many_line_file(repo.path(), 1200);
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path, window, cx);
        });
        cx.run_until_parked();

        let gutter_at_100 = cx
            .debug_bounds("file-view-gutter-1")
            .expect("line 1's gutter should have really painted at the default 100% zoom");
        let text_at_100 = cx
            .debug_bounds("file-view-text-row-1")
            .expect("line 1's text row should have really painted at the default 100% zoom");

        app.update(cx, |app, cx| {
            for _ in 0..5 {
                app.zoom_in(cx); // 100% -> 150%
            }
        });
        cx.run_until_parked();

        let gutter_at_150 = cx
            .debug_bounds("file-view-gutter-1")
            .expect("line 1's gutter should have really painted at 150% zoom");
        let text_at_150 = cx
            .debug_bounds("file-view-text-row-1")
            .expect("line 1's text row should have really painted at 150% zoom");

        assert_eq!(
            gutter_at_100.size.width, gutter_at_150.size.width,
            "the real, fixed-px() line-number gutter must measure identically at every zoom \
             level - it must never respond to the scoped rem-size override"
        );
        assert!(
            text_at_150.size.height > text_at_100.size.height,
            "the real, rems()-sized text row must actually grow taller at 150% zoom \
             (line-height is rems(1.6), scoped to the real effective zoom rem size) - got \
             {:?} at 100% vs {:?} at 150%",
            text_at_100.size,
            text_at_150.size,
        );

        // Scroll a 4-digit line number into view, push zoom to the 200% maximum (the audit
        // measured a wrapped-line-number row at 54px into a 27px slot at 130%, 83px into 41.5px
        // at 200%), and confirm the gutter never grew taller than its row's code text.
        app.update(cx, |app, cx| {
            for _ in 0..5 {
                app.zoom_in(cx); // 150% -> 200%
            }
            app.file_view_scroll_handle
                .scroll_to_item(999, ScrollStrategy::Center);
            cx.notify();
        });
        cx.run_until_parked();

        let gutter_at_200 = cx.debug_bounds("file-view-gutter-1000").expect(
            "scrolling to line 1000 (index 999) at 200% zoom should have really painted its \
             gutter",
        );
        let text_at_200 = cx.debug_bounds("file-view-text-row-1000").expect(
            "scrolling to line 1000 (index 999) at 200% zoom should have really painted its \
             text row",
        );

        assert_eq!(
            gutter_at_200.size.height, text_at_200.size.height,
            "line 1000's real, 4-digit gutter must measure exactly as tall as its own code \
             text row at 200% zoom - a taller gutter means its line number wrapped onto a \
             second real line inside the still-fixed-52px column, which uniform_list's own \
             single-row-height measurement (taken from line 1 alone) would paint straight into \
             the row below's slot, exactly the real overlap the audit measured live"
        );
    }
}
