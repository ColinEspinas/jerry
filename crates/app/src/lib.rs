//! `app`: the ADE desktop application shell.
//!
//! A three-pane GPUI window: a left sidebar listing the target repository's real git
//! worktrees (via `wt-core`) with session/tab controls for spawning agent CLIs or shells
//! into them, a tabbed center pane of real terminal sessions (via `pty-core` +
//! `alacritty_terminal`), and a right sidebar showing the active worktree's real file tree
//! (via `std::fs::read_dir`). See `crate::root`, `crate::sessions`, `crate::terminal_pane`,
//! and `crate::terminal_grid` for the interesting design decisions (entity/state model,
//! blocking-call offloading, terminal grid rendering).

pub mod changes;
pub mod code_view;
pub mod diagnostics_view;
pub mod env_info;
pub mod file_tree;
pub mod fonts;
pub mod hover_view;
pub mod keymap;
pub mod layout;
pub mod merge;
pub mod palette;
pub mod rail;
pub mod root;
pub mod sessions;
pub mod settings;
pub mod settings_store;
pub mod status;
pub mod terminal_grid;
pub mod terminal_links;
pub mod terminal_pane;
pub mod theme;
pub mod work_surface;
pub mod worktrees;

use std::path::PathBuf;

use gpui::{
    px, size, App, AppContext, Bounds, Size, TitlebarOptions, WindowBounds, WindowDecorations,
    WindowOptions,
};

/// The app's globally-bound keyboard shortcuts - the single list both [`run`] (production
/// startup) and this crate's own regression test
/// (`root::focus::palette_focus_tests::secondary_keystroke_opens_the_palette`) bind, so the two
/// can never silently drift apart.
///
/// `"secondary-"`, not `"cmd-"`, is deliberate and was the fix for a live-reproduced bug: GPUI's
/// keystroke parser (`vendor/zed/crates/gpui/src/platform/keystroke.rs:127-159`) treats
/// `"cmd"`/`"super"`/`"win"` as three spellings of the *same* alias, which always sets
/// `modifiers.platform` regardless of OS - on Linux/Windows that's the Super/Windows key, never
/// Ctrl. Binding `"cmd-k"` left the shortcut on Super+K while `crate::keymap`'s rendering
/// (correctly) showed a `Ctrl` keycap on those platforms: `Ctrl+K` did nothing, and `Ctrl+,`
/// fell through to whatever had keyboard focus and typed a literal `,` into it (e.g. a live
/// terminal session).
///
/// `"secondary"` (same file, lines 143-150) is GPUI's own answer to exactly this: it resolves to
/// the `platform` modifier on macOS and `control` everywhere else, at compile time - the same OS
/// fact `crate::keymap::detected_platform_is_macos` resolves for rendering, so this is one
/// source of truth by construction. `f12` is untouched: it's the same physical key on every OS
/// (confirmed against `vendor/zed/assets/keymaps/default-linux.json`'s own
/// `"f12": "editor::GoToDefinition"` binding), so no per-platform alias applies.
///
/// This list is bound once, at app startup, fixed by the compiled-in `target_os`;
/// `crate::keymap::WindowControlsStyle`'s runtime title-bar/keycap override can't change which
/// physical key it matches (see that type's own docs for why).
///
/// A few entries need their own rationale:
/// - `"ctrl-shift-t"` (New terminal in worktree) is deliberately a literal Ctrl on every OS,
///   including macOS, matching the mockup's own `ctrl+shift+T` spec - unlike every other binding
///   here, which uses `"secondary-"`.
/// - The `+` menu's "Open file…" row has **no** global keybinding, despite the mockup's own
///   `mod+P` spec - a real conflict found in audit: `crate::terminal_pane::keystroke_to_bytes`
///   maps an unmodified `Ctrl+<letter>` to the terminal control byte a focused shell expects, and
///   Ctrl+P (`0x10`) is a standard readline binding (`previous-history`) shells rely on. GPUI
///   dispatches a matched `KeyBinding`'s action before a focused element's own `on_key_down`, so
///   a global `"secondary-p"` would swallow that keystroke in every focused terminal on
///   Linux/Windows - the same "app-level shortcut steals terminal input" bug class already fixed
///   once for `secondary-,`. Unlike `"]"` below, there's no narrower `key_context` available
///   (the palette must be openable from any focus target, including a focused terminal). The `+`
///   menu row itself is still a working, click-only way to open the palette scoped to files.
/// - `"]"` (Next changed file) has no modifier, and is scoped to `Some("diff")` rather than
///   global - the only one of this app's bindings with a non-`'rail'`/`'session'` context. A
///   global `"]"` would swallow a literal `]` typed into any focused terminal/agent session
///   (closing a bracket, an array literal, a regex class) - the same bug class as above. Scoping
///   to `"diff"` (`crate::root::code_surface`'s `.key_context("diff")` on the Surface C
///   container) means it only fires while a file tab already has focus, matching the design's
///   intent: `]` cycles *through an already-open review*, not a global "jump into reviewing"
///   shortcut.
/// - `"secondary-1"` through `"secondary-8"` back the tab strip's session-jump keycaps
///   (`root::AdeApp::jump_to_session_at`), expanding the design's `mod+1…8` spec into eight
///   individually bound keystrokes since GPUI has no "N" wildcard keystroke component.
pub fn default_key_bindings() -> Vec<gpui::KeyBinding> {
    vec![
        gpui::KeyBinding::new("secondary-n", root::NewSession, None),
        gpui::KeyBinding::new("secondary-k", root::TogglePalette, None),
        gpui::KeyBinding::new("secondary-,", root::ToggleSettings, None),
        gpui::KeyBinding::new("f12", root::GotoDefinition, None),
        gpui::KeyBinding::new("ctrl-shift-t", root::NewTerminal, None),
        gpui::KeyBinding::new("secondary-shift-n", root::NewAgentPane, None),
        gpui::KeyBinding::new("]", root::NextChangedFile, Some("diff")),
        gpui::KeyBinding::new("secondary-1", root::JumpToSession1, None),
        gpui::KeyBinding::new("secondary-2", root::JumpToSession2, None),
        gpui::KeyBinding::new("secondary-3", root::JumpToSession3, None),
        gpui::KeyBinding::new("secondary-4", root::JumpToSession4, None),
        gpui::KeyBinding::new("secondary-5", root::JumpToSession5, None),
        gpui::KeyBinding::new("secondary-6", root::JumpToSession6, None),
        gpui::KeyBinding::new("secondary-7", root::JumpToSession7, None),
        gpui::KeyBinding::new("secondary-8", root::JumpToSession8, None),
    ]
}

/// Opens the ADE window against `repo_path` and runs the GPUI event loop until the window is
/// closed. Blocks the calling thread for the application's lifetime, mirroring
/// `gpui::Application::run`'s own contract (`vendor/zed/crates/gpui/examples/hello_world.rs`).
pub fn run(repo_path: PathBuf) {
    // `with_assets` registers `fonts::Assets` as the app's `AssetSource`
    // (`vendor/zed/crates/gpui/src/app.rs:198`) before the launch callback runs, since
    // `fonts::load_embedded_fonts` needs `cx.asset_source()` already wired up.
    gpui_platform::application()
        .with_assets(fonts::Assets)
        .run(move |cx: &mut App| {
            if let Err(err) = fonts::load_embedded_fonts(cx) {
                // Not fatal - GPUI falls back to a system font (see `theme::font`'s docs) -
                // but it's a regression from "the bundled Plex glyphs actually render", so it
                // must be visible in the log rather than silently swallowed.
                log::error!("failed to load bundled fonts: {err}");
            }

            cx.bind_keys(default_key_bindings());

            let bounds = Bounds::centered(None, size(px(1440.0), px(928.0)), cx);
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // The design's title-bar band (`crate::root::title_bar`) draws its own
                    // close/minimize/maximize controls, so the OS/compositor shouldn't also
                    // draw a native titlebar - matching `vendor/zed/crates/zed/src/zed.rs`'s
                    // own `titlebar`/`window_decorations` combination.
                    titlebar: Some(TitlebarOptions {
                        title: None,
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    window_decorations: Some(WindowDecorations::Client),
                    window_min_size: Some(Size {
                        width: px(720.0),
                        height: px(480.0),
                    }),
                    ..Default::default()
                },
                {
                    let repo_path = repo_path.clone();
                    move |window, cx| cx.new(|cx| root::AdeApp::new(repo_path.clone(), window, cx))
                },
            );

            match opened {
                Ok(_) => cx.activate(true),
                Err(err) => {
                    // `open_window` failing (e.g. no display available) can't be propagated
                    // through GPUI's `FnOnce(&mut App)` launch callback; log and quit instead
                    // of panicking or leaving a headless process running with no window.
                    log::error!("failed to open ADE window: {err}");
                    cx.quit();
                }
            }
        });
}
