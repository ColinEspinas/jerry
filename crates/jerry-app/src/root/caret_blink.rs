//! Real, shared caret blink (GitHub issue #27: "caret blinks when idle, at a standard cadence
//! (~500ms on/off)... blinking stops while typing and resumes after a short idle delay... blink
//! pauses when the editor loses focus"). Ported from `vendor` GPUI's own blessed idiom for this
//! exact feature - `crates/gpui/examples/view_example/example_editor.rs`'s `Editor::
//! cursor_visible`/`start_blink`/`stop_blink`/`spawn_blink_task`/`reset_blink` (real, runnable
//! example code checked at the pinned `gpui` git dependency's own revision, per this project's
//! own "verify GPUI API usage" discipline - not invented from scratch), adapted for one shared
//! flag on [`AdeApp`] instead of one per input: exactly one of this app's several caret-bearing
//! `FocusHandle`s ([`AdeApp::code_focus_handle`], [`AdeApp::merge_edit_focus_handle`]) can be
//! focused at a time (they're all handles into the same window), so a single blink flag observed
//! by whichever surface is actually focused right now is enough - no need for N independent
//! 500ms timers when at most one caret is ever visible.

use super::*;

/// The idle-blink cadence - issue #27's own "~500ms on/off" (530ms rather than an exact 500 to
/// avoid an unfortunate but harmless beat with `crate::root::FILE_FRESHNESS_CHECK_INTERVAL`'s
/// own 500ms poll in any test that happens to advance a shared test clock by exact multiples of
/// both).
pub(crate) const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(530);

impl AdeApp {
    /// Wires every real caret-bearing `FocusHandle` this app has to the shared blink loop -
    /// called from [`Self::new_with_settings`], which is also the only place with the
    /// `&mut Window` these subscriptions need to register. Returns the subscriptions (and the
    /// handles they cover, for [`Self::caret_blink_handles`]) rather than pushing them onto
    /// `self` directly so the constructor can build the whole `Self` literal in one shot,
    /// matching every other `_subscription`-holding field's own construction pattern in this
    /// codebase (e.g. [`Self::_window_appearance_subscription`]).
    pub(crate) fn wire_caret_blink(
        handles: &[&FocusHandle],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Vec<Subscription>, Vec<FocusHandle>) {
        let mut subscriptions = Vec::with_capacity(handles.len() * 2);
        for handle in handles {
            subscriptions.push(cx.on_focus(handle, window, |this, _window, cx| {
                this.start_caret_blink(cx);
            }));
            subscriptions.push(cx.on_blur(handle, window, |this, window, cx| {
                this.stop_caret_blink_on_blur(window, cx);
            }));
        }
        (
            subscriptions,
            handles.iter().map(|handle| (*handle).clone()).collect(),
        )
    }

    /// A real focus gain on a caret-bearing surface: the caret starts solid and the idle timer
    /// begins from zero.
    pub(crate) fn start_caret_blink(&mut self, cx: &mut Context<Self>) {
        self.caret_blink_visible = true;
        self._caret_blink_task = self.spawn_blink_task(cx);
        cx.notify();
    }

    /// A real focus loss: the loop stops outright (no timer keeps running against an unfocused
    /// caret) - each surface's own render call site is responsible for hiding the caret entirely
    /// while unfocused (GitHub issue #107), not this module.
    pub(crate) fn stop_caret_blink(&mut self, cx: &mut Context<Self>) {
        self.caret_blink_visible = false;
        self._caret_blink_task = Task::ready(());
        cx.notify();
    }

    /// The blur half of [`Self::wire_caret_blink`], and the reason that wiring can't just call
    /// [`Self::stop_caret_blink`] outright.
    pub(crate) fn stop_caret_blink_on_blur(&mut self, window: &Window, cx: &mut Context<Self>) {
        let another_caret_is_focused = window.is_window_active()
            && self
                .caret_blink_handles
                .iter()
                .any(|handle| handle.is_focused(window));
        if another_caret_is_focused {
            return;
        }
        self.stop_caret_blink(cx);
    }

    /// Called after every real cursor-moving action or edit in a caret-bearing surface - forces
    /// the caret back to solid immediately (issue #27's "solid mid-keystroke") and restarts the
    /// idle timer from zero, so a fast typist never sees a mid-word blink. A no-op-looking call
    /// while genuinely unfocused is harmless: the respawned timer still only flips
    /// `caret_blink_visible`, which an unfocused render call site never reads anyway.
    pub(crate) fn reset_caret_blink(&mut self, cx: &mut Context<Self>) {
        self.caret_blink_visible = true;
        self._caret_blink_task = self.spawn_blink_task(cx);
    }

    /// The real, periodic loop - ported from `example_editor.rs`'s own `spawn_blink_task` (see
    /// this module's own docs), gated on
    /// `crate::settings::store::AppearanceSettings::caret_blink`/`gpui::App::reduce_motion`
    /// first: either one being set means "stay solid", so no timer is spawned at all rather than
    /// one that would immediately race a check on every tick.
    fn spawn_blink_task(&mut self, cx: &mut Context<Self>) -> Task<()> {
        if !self.settings.appearance.caret_blink || cx.reduce_motion() {
            return Task::ready(());
        }
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(CARET_BLINK_INTERVAL).await;
            let updated = this.update(cx, |this, cx| {
                this.caret_blink_visible = !this.caret_blink_visible;
                cx.notify();
            });
            if updated.is_err() {
                // The real entity is gone (window/app closed mid-flight) - matches
                // `example_editor.rs`'s own identical early-return on an `Err` update.
                break;
            }
        })
    }
}

/// Real regression coverage for the live-reported "carets across all inputs, sometimes it does
/// not display, sometimes just blinks once and never displays again" bug - see
/// [`AdeApp::stop_caret_blink_on_blur`]'s own docs for the root cause (GPUI fans one
/// `WindowFocusEvent` out to every focus listener in *registration* order, so a focus change to
/// an earlier-wired handle ran `start_caret_blink` before `stop_caret_blink` and left the shared
/// loop dead).
#[cfg(test)]
mod caret_blink_focus_order_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};

    /// A real repo directory with a real file in it, opened in a real File view, so
    /// [`AdeApp::code_focus_handle`] - the *first* handle `AdeApp::wire_caret_blink` subscribes,
    /// and therefore the one every ordering failure lands on - is genuinely rendered and
    /// `track_focus`'d (`crate::code_surface::render`).
    fn open_app_with_a_focusable_editor(
        cx: &mut TestAppContext,
    ) -> (
        Entity<AdeApp>,
        &mut VisualTestContext,
        crate::test_support::TempRoot,
    ) {
        let repo = crate::test_support::temp_root();
        let file_path = repo.path().join("notes.txt");
        std::fs::write(&file_path, "hello\n").expect("write notes.txt");
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        // `on_focus`/`on_blur` only fire while GPUI considers the window itself active - a
        // freshly opened test window starts out inactive, so this is a real precondition, not
        // scaffolding (the same note every other caret-blink test in this codebase carries).
        app.update_in(cx, |_app, window, _cx| window.activate_window());
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        (app, cx, repo)
    }

    /// Advances the real clock past one full blink interval and reports the flag afterwards -
    /// the only way to tell a live loop from a dead `Task::ready(())`.
    fn tick(app: &Entity<AdeApp>, cx: &mut VisualTestContext) -> bool {
        cx.background_executor
            .advance_clock(CARET_BLINK_INTERVAL + Duration::from_millis(50));
        cx.run_until_parked();
        app.read_with(cx, |app, _| app.caret_blink_visible)
    }

    #[gpui::test]
    fn moving_focus_to_an_earlier_wired_handle_keeps_the_blink_loop_alive(cx: &mut TestAppContext) {
        let (app, cx, _repo) = open_app_with_a_focusable_editor(cx);

        // Into the rail filter first - a *later*-wired handle, so this direction always worked
        // and is a real precondition rather than part of what is being proven.
        app.update_in(cx, |app, window, cx| {
            window.focus(&app.filter_focus_handle, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "sanity check: focusing the rail filter must start the shared loop solid"
        );
        assert!(
            !tick(&app, cx),
            "sanity check: and that loop must really be ticking"
        );

        app.update_in(cx, |app, window, cx| {
            app.focus_code_surface(window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, _cx| {
            assert!(
                app.code_focus_handle.is_focused(window),
                "sanity check: focus must really have landed on the editor"
            );
        });

        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "moving focus back into the editor must leave its caret solid - before this fix the \
             editor's own `on_focus` ran first and the rail filter's `on_blur` then pinned the \
             shared flag straight back off, so the caret never appeared at all"
        );
        assert!(
            !tick(&app, cx),
            "and the shared blink task must still be live - before this fix that `on_blur` \
             replaced it with `Task::ready(())`, so the caret stayed invisible forever"
        );
        assert!(
            tick(&app, cx),
            "and keep its cadence across a second interval, not just fire once"
        );
    }

    #[gpui::test]
    fn dismissing_the_palette_leaves_the_editor_caret_blinking(cx: &mut TestAppContext) {
        let (app, cx, _repo) = open_app_with_a_focusable_editor(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_palette(window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "sanity check: the palette's own input must start the shared loop solid"
        );

        app.update_in(cx, |app, window, cx| {
            app.close_palette(window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, _cx| {
            assert!(
                app.code_focus_handle.is_focused(window),
                "sanity check: closing the palette must restore focus to the editor"
            );
        });

        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "dismissing the palette must hand the caret back to the editor solid, not pin it off"
        );
        assert!(
            !tick(&app, cx),
            "and hand back a live loop, not a dead `Task::ready(())`"
        );
        assert!(tick(&app, cx), "which keeps its cadence");
    }

    #[gpui::test]
    fn blurring_to_a_non_caret_target_still_stops_the_blink_loop(cx: &mut TestAppContext) {
        let (app, cx, _repo) = open_app_with_a_focusable_editor(cx);

        app.update_in(cx, |app, window, cx| {
            window.focus(&app.filter_focus_handle, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "sanity check: the rail filter must start the shared loop solid"
        );

        app.update_in(cx, |app, window, cx| {
            window.focus(&app.rail_focus_handle, cx);
        });
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.caret_blink_visible),
            "leaving every caret-bearing surface must still pin the shared flag off immediately"
        );
        assert!(
            !tick(&app, cx),
            "and must leave no timer running behind it - a still-live loop would flip this back \
             on with no caret focused at all"
        );
    }

    #[gpui::test]
    fn deactivating_the_window_still_stops_the_blink_loop(cx: &mut TestAppContext) {
        let (app, cx, _repo) = open_app_with_a_focusable_editor(cx);

        app.update_in(cx, |app, window, cx| {
            window.focus(&app.filter_focus_handle, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "sanity check: the rail filter must start the shared loop solid"
        );

        cx.deactivate_window();
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.caret_blink_visible),
            "a window going inactive must still stop the caret blinking, even though its focus \
             handle is technically still the focused one"
        );
        assert!(
            !tick(&app, cx),
            "and must leave no timer running in the background window"
        );
    }
}
