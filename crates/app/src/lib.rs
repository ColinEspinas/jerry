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
pub mod layout;
pub mod merge;
pub mod palette;
pub mod rail;
pub mod root;
pub mod sessions;
pub mod settings;
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
            // focused terminal tab was verified). `cmd-k` follows the exact same
            // `actions!`/`KeyBinding` pattern for the command palette (see
            // `crate::root::TogglePalette`'s docs). `cmd-,` follows it again for the
            // Settings surface (see `crate::root::ToggleSettings`'s docs for the real
            // `vendor/zed/assets/keymaps/default-macos.json` precedent this literal
            // keystroke string was verified against).
            cx.bind_keys([
                gpui::KeyBinding::new("cmd-n", root::NewSession, None),
                gpui::KeyBinding::new("cmd-k", root::TogglePalette, None),
                gpui::KeyBinding::new("cmd-,", root::ToggleSettings, None),
            ]);

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
