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
pub mod completion_view;
pub mod diagnostics_view;
pub mod edit_buffer;
pub mod env_info;
pub mod file_tree;
pub mod fonts;
pub mod hover_view;
pub mod keymap;
pub mod language;
pub mod layout;
pub mod merge;
pub mod palette;
pub mod process_stats;
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
/// - `"]"` (Next changed file) has no modifier, and is scoped to `Some("diff && !file-editor")`
///   rather than global - the only one of this app's bindings with a non-`'rail'`/`'session'`
///   context. A global `"]"` would swallow a literal `]` typed into any focused terminal/agent
///   session (closing a bracket, an array literal, a regex class) - the same bug class as above.
///   Scoping to `"diff"` (`crate::root::code_surface`'s `.key_context("diff")` on the Surface C
///   container) means it only fires while a file tab already has focus, matching the design's
///   intent: `]` cycles *through an already-open review*, not a global "jump into reviewing"
///   shortcut. The `&& !file-editor` half is a real, live-reproduced fix (not part of the
///   original design): Revision R8.5a's real File view text editing adds a `"file-editor"`
///   context *alongside* `"diff"` on that same container (see the `Editor*` entry below) rather
///   than replacing it, so a bare `Some("diff")` predicate kept matching - and kept swallowing a
///   literal `]` keystroke before it ever reached the real edit buffer - even while a file was
///   actively being edited, reproduced live by typing `]` into real content. `!`/`&&` are real,
///   supported `KeyBindingContextPredicate` syntax (`vendor/zed/crates/gpui/src/keymap/
///   context.rs:172-420`'s own `Not`/`And` variants and parser), not invented here.
/// - `"secondary-1"` through `"secondary-8"` back the tab strip's session-jump keycaps
///   (`root::AdeApp::jump_to_session_at`), expanding the design's `mod+1…8` spec into eight
///   individually bound keystrokes since GPUI has no "N" wildcard keystroke component.
/// - The `Editor*` entries (Revision R8.5a's real File view text editing) are scoped to
///   `Some("file-editor")`, a real *additional* context alongside `"diff"` above - both live on
///   the *same* focused "code-surface" container (`root::code_surface::AdeApp::
///   render_code_surface`'s outer div, the one `code_focus_handle` is actually `track_focus`'d
///   on), with `"file-editor"` only added to that node's own context string (space-separated:
///   `"diff file-editor"`) while the editable File view - not the read-only Diff view - is
///   showing for an open tab with a real `EditBuffer`. GPUI's real key dispatch only bubbles
///   `on_action`/context from the *focused* node up through its ancestors, never down into a
///   descendant, so binding these on a separate inner container (an earlier draft of this code
///   tried exactly that) would never actually fire - see `render_code_surface`'s own docs for the
///   real, live-verified bug this was. The read-only Diff view genuinely never receives a single
///   one of these bindings: its context string never gains `"file-editor"`. Plain letters/arrows
///   are deliberately *not* globally bound (unlike, say, `f12`) - binding them at `None` scope
///   would swallow ordinary typing in every focused terminal session the same way an unscoped
///   `"]"` would have (see that entry's own docs above) - `"file-editor"` is the only context
///   they're ever active in. `EditorSave` is `"secondary-s"`, following this list's own
///   `"secondary-"` convention (verified against this same list: no other entry already claims
///   it). `EditorSaveAnyway` (`"secondary-shift-s"`) is the real, explicit, opt-in override for
///   an `AdeApp::file_external_conflict` - see `root::editing::AdeApp::force_save_active_file`'s
///   own docs for the real permanent-deadlock bug (a conflict that, once flagged, could never
///   clear again through the ordinary save path) this exists to let the user deliberately break
///   out of.
/// - The `Completions*` entries (Revision R8.5b) back the real Completions popup
///   (`crate::root::completions`), scoped to `Some("file-editor && completions")` - a real,
///   narrower sibling context added to the same node only while the popup is genuinely open (see
///   `crate::root::code_surface::AdeApp::render_code_surface`'s own docs). `enter`/`up`/`down`
///   above are correspondingly narrowed to `!completions` so the two mutually-exclusive predicate
///   sets can never both match the same keystroke - the same real `&&`/`!`
///   `KeyBindingContextPredicate` mechanism the `"]"` entry already established for this exact
///   bug class.
pub fn default_key_bindings() -> Vec<gpui::KeyBinding> {
    vec![
        gpui::KeyBinding::new("secondary-n", root::NewSession, None),
        gpui::KeyBinding::new("secondary-k", root::TogglePalette, None),
        gpui::KeyBinding::new("secondary-,", root::ToggleSettings, None),
        gpui::KeyBinding::new("f12", root::GotoDefinition, None),
        gpui::KeyBinding::new("ctrl-shift-t", root::NewTerminal, None),
        gpui::KeyBinding::new("secondary-shift-n", root::NewAgentPane, None),
        gpui::KeyBinding::new("]", root::NextChangedFile, Some("diff && !file-editor")),
        gpui::KeyBinding::new("secondary-1", root::JumpToSession1, None),
        gpui::KeyBinding::new("secondary-2", root::JumpToSession2, None),
        gpui::KeyBinding::new("secondary-3", root::JumpToSession3, None),
        gpui::KeyBinding::new("secondary-4", root::JumpToSession4, None),
        gpui::KeyBinding::new("secondary-5", root::JumpToSession5, None),
        gpui::KeyBinding::new("secondary-6", root::JumpToSession6, None),
        gpui::KeyBinding::new("secondary-7", root::JumpToSession7, None),
        gpui::KeyBinding::new("secondary-8", root::JumpToSession8, None),
        gpui::KeyBinding::new("backspace", root::EditorBackspace, Some("file-editor")),
        gpui::KeyBinding::new("delete", root::EditorDelete, Some("file-editor")),
        // Narrowed to `!completions` (Revision R8.5b) - see the `Completions*` entries below and
        // this list's own docs for why: while the real Completions popup is open, `Enter`/`Up`/
        // `Down` must reach `CompletionsAccept`/`CompletionsUp`/`CompletionsDown` instead of
        // inserting a newline/moving the real caret.
        gpui::KeyBinding::new(
            "enter",
            root::EditorEnter,
            Some("file-editor && !completions"),
        ),
        gpui::KeyBinding::new("left", root::EditorLeft, Some("file-editor")),
        gpui::KeyBinding::new("right", root::EditorRight, Some("file-editor")),
        gpui::KeyBinding::new("up", root::EditorUp, Some("file-editor && !completions")),
        gpui::KeyBinding::new(
            "down",
            root::EditorDown,
            Some("file-editor && !completions"),
        ),
        gpui::KeyBinding::new("shift-left", root::EditorSelectLeft, Some("file-editor")),
        gpui::KeyBinding::new("shift-right", root::EditorSelectRight, Some("file-editor")),
        gpui::KeyBinding::new("shift-up", root::EditorSelectUp, Some("file-editor")),
        gpui::KeyBinding::new("shift-down", root::EditorSelectDown, Some("file-editor")),
        gpui::KeyBinding::new("home", root::EditorHome, Some("file-editor")),
        gpui::KeyBinding::new("end", root::EditorEnd, Some("file-editor")),
        gpui::KeyBinding::new("secondary-a", root::EditorSelectAll, Some("file-editor")),
        gpui::KeyBinding::new("secondary-c", root::EditorCopy, Some("file-editor")),
        gpui::KeyBinding::new("secondary-x", root::EditorCut, Some("file-editor")),
        gpui::KeyBinding::new("secondary-v", root::EditorPaste, Some("file-editor")),
        gpui::KeyBinding::new("secondary-s", root::EditorSave, Some("file-editor")),
        gpui::KeyBinding::new(
            "secondary-shift-s",
            root::EditorSaveAnyway,
            Some("file-editor"),
        ),
        // Real Completions popup navigation/accept/dismiss (Revision R8.5b) - scoped to
        // `"file-editor && completions"`, the real *narrower* mirror of the `!completions`
        // narrowing on `enter`/`up`/`down` above, added to the same code-surface node only while
        // `AdeApp::completions` is genuinely, actionably `Ready` for the active file (see
        // `crate::root::code_surface::AdeApp::render_code_surface`'s own docs for exactly where
        // that context tag comes from, and `crate::root::completions::AdeApp::
        // completions_open_for_active_path`'s own docs for why `Loading`/`Failed` don't count).
        // `Tab` has no competing plain-`Editor*` *action* binding anywhere in this list - but the
        // real, live-verified reason scoping it only to `"file-editor && completions"` is safe
        // isn't "nothing else claims it": a real, live keystroke test confirms that with the
        // popup closed, an unbound `tab` keystroke still reaches the real edit buffer and inserts
        // a literal `\t`, the same way any other unbound printable character does - GPUI falls
        // through to the platform's ordinary text-input/IME path (`crate::root::editing`'s
        // `EntityInputHandler::replace_text_in_range`) for any keystroke with no matching
        // `KeyBinding` in the currently active context, rather than dropping it. So `Tab` is
        // never actually *unhandled* outside this narrow context; it's handled by a different,
        // pre-existing real mechanism than a `KeyBinding`/action at all.
        gpui::KeyBinding::new(
            "up",
            root::CompletionsUp,
            Some("file-editor && completions"),
        ),
        gpui::KeyBinding::new(
            "down",
            root::CompletionsDown,
            Some("file-editor && completions"),
        ),
        gpui::KeyBinding::new(
            "tab",
            root::CompletionsAccept,
            Some("file-editor && completions"),
        ),
        gpui::KeyBinding::new(
            "enter",
            root::CompletionsAccept,
            Some("file-editor && completions"),
        ),
        gpui::KeyBinding::new(
            "escape",
            root::CompletionsDismiss,
            Some("file-editor && completions"),
        ),
        // Surface D's merge hand-edit whole-file editor (Revision R8.5c,
        // `crate::root::merge_editing`) - a distinct `"merge-editor"` context, deliberately never
        // `"file-editor"` itself: the same real `Editor*` action *types*/handler bodies are
        // reused (see `crate::root::editing::AdeApp::active_edit_target`'s own docs for how a
        // handler routes to whichever buffer is actually the current target), but reusing
        // `"file-editor"` verbatim would also pull in the `"... && completions"`/
        // `"... && !completions"` narrowing above, which makes no sense for a merge buffer - no
        // completions popup is ever wired up for it (see `crate::root::merge_editing`'s own top
        // docs). `EditorSaveAnyway` (`secondary-shift-s`) is deliberately *not* bound here either:
        // there is no external-change-conflict concept for a merge hand-edit buffer (see
        // `crate::root::merge_flow::AdeApp::save_merge_edit`'s own docs).
        gpui::KeyBinding::new("backspace", root::EditorBackspace, Some("merge-editor")),
        gpui::KeyBinding::new("delete", root::EditorDelete, Some("merge-editor")),
        gpui::KeyBinding::new("enter", root::EditorEnter, Some("merge-editor")),
        gpui::KeyBinding::new("left", root::EditorLeft, Some("merge-editor")),
        gpui::KeyBinding::new("right", root::EditorRight, Some("merge-editor")),
        gpui::KeyBinding::new("up", root::EditorUp, Some("merge-editor")),
        gpui::KeyBinding::new("down", root::EditorDown, Some("merge-editor")),
        gpui::KeyBinding::new("shift-left", root::EditorSelectLeft, Some("merge-editor")),
        gpui::KeyBinding::new("shift-right", root::EditorSelectRight, Some("merge-editor")),
        gpui::KeyBinding::new("shift-up", root::EditorSelectUp, Some("merge-editor")),
        gpui::KeyBinding::new("shift-down", root::EditorSelectDown, Some("merge-editor")),
        gpui::KeyBinding::new("home", root::EditorHome, Some("merge-editor")),
        gpui::KeyBinding::new("end", root::EditorEnd, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-a", root::EditorSelectAll, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-c", root::EditorCopy, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-x", root::EditorCut, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-v", root::EditorPaste, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-s", root::EditorSave, Some("merge-editor")),
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
