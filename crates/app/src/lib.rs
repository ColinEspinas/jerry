//! `app`: the ADE desktop application shell.
//!
//! A three-pane GPUI window: a left sidebar listing the target repository's real git
//! worktrees (via `wt-core`), a center pane running a real shell for the selected worktree
//! (via `pty-core`), and a right sidebar showing that worktree's real file tree (via
//! `std::fs::read_dir`). See `crate::root`, `crate::terminal_pane`, and `crate::ansi` for
//! the interesting design decisions (entity/state model, blocking-call offloading, terminal
//! rendering fidelity trade-off).

pub mod ansi;
pub mod file_tree;
pub mod root;
pub mod terminal_pane;
pub mod worktrees;

use std::path::PathBuf;

use gpui::{px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};

/// Opens the ADE window against `repo_path` and runs the GPUI event loop until the window
/// is closed. Blocks the calling thread for the lifetime of the application, mirroring
/// `gpui::Application::run`'s own contract (verified at
/// `vendor/zed/crates/gpui/examples/hello_world.rs`).
pub fn run(repo_path: PathBuf) {
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(700.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            {
                let repo_path = repo_path.clone();
                move |_window, cx| cx.new(|cx| root::AdeApp::new(repo_path.clone(), cx))
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
