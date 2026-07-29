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
pub mod terminal_pane;
pub mod theme;
pub mod work_surface;
pub mod worktrees;

use std::path::PathBuf;

use gpui::{
    px, size, App, AppContext, Bounds, Size, TitlebarOptions, WindowBounds, WindowDecorations,
    WindowOptions,
};

/// The app's real, globally-bound keyboard shortcuts - the single list both [`run`] (production
/// startup) and this crate's own regression test
/// (`root::focus::palette_focus_tests::secondary_keystroke_opens_the_palette`) bind, so the two
/// can never silently drift apart the way a copy-pasted duplicate list could.
///
/// `"secondary-"`, not `"cmd-"`, is deliberate and was the fix for a real, live-reproduced bug:
/// GPUI's keystroke parser (verified at `vendor/zed/crates/gpui/src/platform/keystroke.rs:127-
/// 159`) treats `"cmd"`/`"super"`/`"win"` as three spellings of the *same* alias, which always
/// sets `modifiers.platform` regardless of OS - on Linux/Windows that's the Super/Windows key,
/// never Ctrl. Binding `"cmd-k"` therefore left the real shortcut on Super+K while this app's own
/// `crate::keymap` rendering (correctly) shows a `Ctrl` keycap on those platforms - a real,
/// demonstrated mismatch: `Ctrl+K` did nothing, and `Ctrl+,` fell through to whatever had
/// keyboard focus and typed a literal `,` into it (e.g. a live terminal session), since nothing
/// intercepted it as an app-level action.
///
/// `"secondary"` (same file, lines 143-150) is GPUI's own documented answer to exactly this: it
/// resolves to the `platform` modifier on macOS (`cfg!(target_os = "macos")`) and `control`
/// everywhere else at compile time - the same OS fact `crate::keymap::detected_platform_is_macos`
/// resolves for rendering, so this is one source of truth by construction, not two that could
/// disagree. `f12` is untouched: it's the same physical key on every OS, so no per-platform alias
/// applies - confirmed against `vendor/zed/assets/keymaps/default-linux.json`'s own real
/// `"f12": "editor::GoToDefinition"` binding.
///
/// Note this list is bound once, at real app startup, and is fixed by the *real* compiled-in
/// `target_os` - `crate::keymap::WindowControlsStyle`'s runtime title-bar/keycap override can't
/// change which physical key this list matches (see that type's own docs for why, and why that's
/// an honest, documented limitation rather than a bug).
///
/// ## The `+` menu / tab-strip bindings added alongside this (Revision R4a)
///
/// `"ctrl-shift-t"` is deliberately *not* `"secondary-shift-t"` - the mockup's own
/// `Jerry.dc.html` `keymapDefs`/`plusItems` fixture literally specs `New terminal in worktree` as
/// `ctrl+shift+T` (`{ cmd: 'New terminal in worktree', spec: 'ctrl+shift+T', ctx: 'session', src:
/// 'user' }`), not `mod+shift+T` - a real, literal Ctrl on every OS including macOS, unlike every
/// other binding in this list. `"ctrl"` is confirmed as GPUI's real, always-`modifiers.control`
/// component (distinct from `"secondary"`'s per-OS alias - see this function's own docs, above)
/// at `vendor/zed/crates/gpui/src/platform/keystroke.rs`'s `Keystroke::parse`.
///
/// `"secondary-shift-n"` (New agent pane) does use the real `"secondary-"` alias, per the
/// mockup's own `mod+shift+N` spec for that row.
///
/// The `+` menu's fourth row, "Open file…", has **no** real global keybinding at all, despite the
/// mockup's own `mod+P` spec for it - a real, live-reproduced conflict found in audit, not an
/// oversight: `crate::terminal_pane::keystroke_to_bytes` maps an unmodified `Ctrl+<letter>`
/// keystroke to the real terminal control byte a focused shell expects
/// (`letter.to_ascii_uppercase() as u8 & 0x1f`), and Ctrl+P (`0x10`) is a real, standard readline
/// binding (`previous-history`) every real shell relies on. GPUI dispatches a matched, registered
/// `KeyBinding`'s action *before* a focused element's own `on_key_down`
/// (`vendor/zed/crates/gpui/src/window.rs`'s `dispatch_key_event`), so a *global* `"secondary-p"`
/// binding here would silently swallow that real keystroke in every focused terminal/agent
/// session on Linux/Windows (where `"secondary"` resolves to plain `Ctrl`), breaking real shell
/// history navigation - the same real "app-level shortcut steals real terminal input" bug class
/// this crate's own R2 audit already found and fixed once for `secondary-,`. Unlike `"]"` (below),
/// there's no `key_context` that can scope this the same way: the palette has to be openable from
/// *any* real focus target, including a focused terminal (that's exactly what `"secondary-k"`
/// already does, and still does), so there's no narrower context to require. The `+` menu row
/// itself is still a real, working, click-only way to open the palette scoped to files - see
/// `root::AdeApp::render_plus_menu`'s own docs.
///
/// `"]"` (Next changed file) has no modifier at all - the mockup's own spec is the bare key,
/// confirmed against `vendor/zed/crates/gpui/src/platform/keystroke.rs`'s `Keystroke::parse`,
/// which accepts a single unrecognized component as the key on its own with no modifiers set.
/// Unlike every other binding here, it's scoped to `Some("diff")` (matching the mockup's own
/// `keymapDefs` entry - `{ cmd: 'Next changed file', spec: ']', ctx: 'diff', src: 'base' }` -
/// the *only* one of this app's real global bindings with a non-`'rail'`/`'session'` context),
/// not `None` (global) - a real, live-verified reason, not just literal spec-following: GPUI
/// dispatches a matched `KeyBinding`'s action *before* a focused element's own `on_key_down`
/// (`vendor/zed/crates/gpui/src/window.rs`'s `dispatch_key_event`, whose `match_result.bindings`
/// loop runs, and unconditionally sets `cx.propagate_event = false` on a real match, ahead of
/// `finish_dispatch_key_event` - the function that actually invokes `on_key_down` listeners like
/// `crate::terminal_pane::TerminalPane::handle_key_down`). A *global* `"]"` binding would
/// therefore have silently swallowed a literal `]` keystroke typed into any live, focused
/// terminal/agent session - closing a bracket, an array literal, a regex character class - and
/// forwarded it to `NextChangedFile` instead, exactly the "app-level shortcut steals real
/// terminal input" bug class this crate's own R2 audit already found and fixed once for
/// `secondary-,`. Scoping to `"diff"` (via `crate::root::code_surface`'s own `.key_context
/// ("diff")` on the Surface C/diff-file container - real, verified at `vendor/zed/crates/gpui/
/// src/elements/div.rs`'s `Styled::key_context`) means it only fires while a real file tab
/// already has focus, which is also a real, defensible reading of the design's own intent: `]`
/// cycles *through an already-open review*, not a global "jump into reviewing from anywhere"
/// shortcut - the `+` menu's own "Next changed file" row (a mouse click, not a keystroke) still
/// works from any tab regardless of this scoping.
///
/// `"secondary-1"` through `"secondary-8"` back the tab strip's real session-jump keycaps
/// (`root::AdeApp::jump_to_session_at`) - the design's own `mod+1…8` spec
/// (`Jerry.dc.html`'s `keymapDefs`: `{ cmd: 'Jump to session by position', spec: 'mod+1…8', ctx:
/// 'rail', src: 'base' }`) expanded into eight real, individually bound keystrokes, since GPUI
/// has no "N" wildcard keystroke component - only eight separate literal bindings actually make
/// each number jump to that position.
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

/// Opens the ADE window against `repo_path` and runs the GPUI event loop until the window
/// is closed. Blocks the calling thread for the lifetime of the application, mirroring
/// `gpui::Application::run`'s own contract (verified at
/// `vendor/zed/crates/gpui/examples/hello_world.rs`).
pub fn run(repo_path: PathBuf) {
    // `with_assets` registers `fonts::Assets` as the app's real `AssetSource` (verified at
    // `vendor/zed/crates/gpui/src/app.rs:198`, the same method Zed's own `main.rs` uses)
    // *before* the launch callback runs, since `fonts::load_embedded_fonts` needs
    // `cx.asset_source()` to already be wired up.
    gpui_platform::application()
        .with_assets(fonts::Assets)
        .run(move |cx: &mut App| {
            if let Err(err) = fonts::load_embedded_fonts(cx) {
                // A font-loading failure shouldn't be fatal - GPUI still falls back to a
                // system font (see `theme::font`'s docs) - but it's a real regression from
                // "the bundled Plex glyphs actually render", so it must be visible in the
                // log rather than silently swallowed.
                log::error!("failed to load bundled fonts: {err}");
            }

            // The rail's `+`/⌘N "new session" control (see `crate::root::NewSession`'s
            // docs for the real, verified `actions!`/`KeyBinding` pattern this follows,
            // taken directly from `vendor/zed/crates/gpui/examples/input.rs`, and for the
            // documented judgment call on how far its focus-priority interaction with a
            // focused terminal tab was verified). `secondary-k` follows the exact same
            // `actions!`/`KeyBinding` pattern for the command palette (see
            // `crate::root::TogglePalette`'s docs). `secondary-,` follows it again for the
            // Settings surface (see `crate::root::ToggleSettings`'s docs). `f12` follows the
            // exact same real `actions!`/`KeyBinding` pattern for the File view's
            // go-to-definition (see `crate::root::GotoDefinition`'s own docs). See
            // `default_key_bindings`'s own docs, above, for why `"secondary-"` (not `"cmd-"`)
            // is the real, verified-correct prefix for the first three.
            cx.bind_keys(default_key_bindings());

            let bounds = Bounds::centered(None, size(px(1440.0), px(928.0)), cx);
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // The design's title-bar band (see `crate::title_bar`) is real window
                    // content that draws its own close/minimize/maximize controls, so the
                    // OS/compositor shouldn't also draw a native titlebar - matching
                    // `vendor/zed/crates/zed/src/zed.rs`'s own `titlebar`/
                    // `window_decorations` combination for its self-drawn titlebar.
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
