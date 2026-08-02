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
//!
//! ## The "no blink" setting and `prefers-reduced-motion`
//!
//! [`AdeApp::settings`]'s `appearance.caret_blink` (see
//! `crate::settings::store::AppearanceSettings::caret_blink`'s own docs) is the real, persisted
//! "no blink" setting the issue asks for: when `false`, [`spawn_blink_task`] never starts a
//! timer at all, so the caret stays permanently solid (still hidden while unfocused, same as the
//! blinking case - see each surface's own render call site).
//!
//! `gpui::App::reduce_motion`/`set_reduce_motion` (`vendor` GPUI's real, available mechanism for
//! "respect reduced motion" - `crates/gpui/src/app.rs:1010,1016` at the pinned revision) is
//! honored the same way: when `cx.reduce_motion()` is true, blink is skipped exactly like
//! `caret_blink == false`. This is a genuine, honest gap worth stating plainly: **nothing in
//! this pinned GPUI version auto-detects the OS's actual `prefers-reduced-motion` preference**
//! (verified directly - `crates/gpui/src/platform/` has no `reduce_motion`/`ReduceMotion`
//! reference anywhere, on any platform backend), and neither does Zed's own upstream code that
//! uses this same flag: `crates/zed/src/zed.rs`'s own `init_reduce_motion` reads *Zed's own
//! settings-file value* and pushes it into `cx.set_reduce_motion` - it is a settings-driven
//! flag there too, not real OS detection. So `cx.reduce_motion()` is the real, correct hook to
//! honor (and this app does), but nothing currently calls `cx.set_reduce_motion(true)` from a
//! real OS signal, because there is no such signal available to call it from at this GPUI
//! revision. The persisted `caret_blink` toggle is this app's own real, concrete answer to the
//! issue's "no blink setting" ask; `reduce_motion` is wired and respected for whenever a future
//! platform-layer signal (or an explicit settings toggle mapped onto it) makes it real.

use super::*;

/// The idle-blink cadence - issue #27's own "~500ms on/off" (530ms rather than an exact 500 to
/// avoid an unfortunate but harmless beat with `crate::root::FILE_FRESHNESS_CHECK_INTERVAL`'s
/// own 500ms poll in any test that happens to advance a shared test clock by exact multiples of
/// both).
pub(crate) const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(530);

impl AdeApp {
    /// Wires every real caret-bearing `FocusHandle` this app has to the shared blink loop -
    /// called exactly once, from [`Self::new_with_settings`], which is also the only place with
    /// the `&mut Window` these subscriptions need to register. Returns the subscriptions rather
    /// than pushing them onto `self` directly so the constructor can build the whole `Self`
    /// literal in one shot, matching every other `_subscription`-holding field's own construction
    /// pattern in this codebase (e.g. [`Self::_window_appearance_subscription`]).
    pub(crate) fn wire_caret_blink(
        handles: &[&FocusHandle],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let mut subscriptions = Vec::with_capacity(handles.len() * 2);
        for handle in handles {
            subscriptions.push(cx.on_focus(handle, window, |this, _window, cx| {
                this.start_caret_blink(cx);
            }));
            subscriptions.push(cx.on_blur(handle, window, |this, _window, cx| {
                this.stop_caret_blink(cx);
            }));
        }
        subscriptions
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
