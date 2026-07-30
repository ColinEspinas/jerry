//! `app`: the ADE desktop application shell.
//!
//! A three-pane GPUI window: a left sidebar listing the target repository's real git
//! worktrees (via `wt-core`) with session/tab controls for spawning agent CLIs or shells
//! into them, a tabbed center pane of real terminal sessions (via `pty-core` +
//! `alacritty_terminal`), and a right sidebar showing the active worktree's real file tree
//! (via `std::fs::read_dir`). See `crate::root`, `crate::work_surface::sessions`, `crate::terminal::pane`,
//! and `crate::terminal::grid` for the interesting design decisions (entity/state model,
//! blocking-call offloading, terminal grid rendering).

pub mod code_surface;
pub mod env_info;
pub mod fonts;
pub mod keymap;
pub mod keymap_overrides;
pub mod language;
pub mod lsp;
pub mod merge;
pub mod palette;
pub mod rail;
pub mod root;
pub mod settings;
pub mod sidebar;
pub mod status_bar;
pub mod terminal;
pub mod text_history;
pub mod theme;
// `pub(crate)`, unlike every other feature folder above: this one exposes no public item at
// all (both its files are `pub(crate) mod`), and `root::title_bar` was a private module before
// the split - so making it `pub` would widen this crate's external surface for no reason.
pub(crate) mod title_bar;
pub mod work_surface;
pub mod worktree_history;

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
///   `mod+P` spec - a real conflict found in audit: `crate::terminal::pane::keystroke_to_bytes`
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
///   Scoping to `"diff"` (`crate::code_surface`'s `.key_context("diff")` on the Surface C
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
///   the *same* focused "code-surface" container (`code_surface::render::AdeApp::
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
///   an `AdeApp::file_external_conflict` - see `code_surface::editing::AdeApp::force_save_active_file`'s
///   own docs for the real permanent-deadlock bug (a conflict that, once flagged, could never
///   clear again through the ordinary save path) this exists to let the user deliberately break
///   out of.
/// - The `Completions*` entries (Revision R8.5b) back the real Completions popup
///   (`crate::lsp::completion_popup`), scoped to `Some("file-editor && completions")` - a real,
///   narrower sibling context added to the same node only while the popup is genuinely open (see
///   `crate::code_surface::render::AdeApp::render_code_surface`'s own docs). `enter`/`up`/`down`
///   above are correspondingly narrowed to `!completions` so the two mutually-exclusive predicate
///   sets can never both match the same keystroke - the same real `&&`/`!`
///   `KeyBindingContextPredicate` mechanism the `"]"` entry already established for this exact
///   bug class.
/// - `Undo`/`Redo` (Revision R10, `crate::worktree_history::flow`) back the real command-pattern
///   undo/redo stack for "keep all changes"/"discard worktree". `"secondary-z"`/
///   `"secondary-shift-z"` follow this list's own `"secondary-"` convention, but - unlike every
///   other entry above except `"]"` - are **not** globally scoped (`None`): `"secondary-z"`
///   resolves to plain `Ctrl+Z` on Linux/Windows, which `crate::terminal::pane::keystroke_to_bytes`
///   already maps to the real `SIGTSTP` control byte (`0x1a`) - the terminal-suspend keystroke
///   essentially every interactive terminal program relies on. A global binding here would
///   swallow it before it ever reached a focused terminal's own key handling, the same
///   "app-level shortcut steals terminal input" bug class this list's own `secondary-p`/`"]"`
///   docs already cover, just for a far more disruptive keystroke to silently lose than either of
///   those. Unlike `secondary-p` (no narrower context existed for it - the palette must be
///   reachable from a focused terminal), a real, narrower scope *is* available here: `Undo`/
///   `Redo` have no legitimate reason to need to fire while a terminal has keyboard focus, so
///   they're scoped to `Some("!terminal")` - `crate::terminal::pane::TerminalPane`'s own
///   `"terminal"` context tag (added in the same revision specifically to make this predicate
///   possible), matching this list's own `"]"`/`"diff && !file-editor"` precedent for the same
///   `!`-negated-context mechanism. **Narrowed again** by GitHub issue #17 to
///   `Some("!terminal && !text-input")` - see the `TextUndo`/`TextRedo` entry directly below.
/// - `TextUndo`/`TextRedo` (GitHub issue #17, `crate::text_history`) are the *second*, genuinely
///   distinct undo system in this app: per-widget **text** undo, as opposed to `Undo`/`Redo`'s
///   worktree-level git history above. They share the same physical keys, which is exactly the
///   situation this list's own docs exist to keep honest, so they are kept apart **structurally**,
///   by mutually-exclusive context predicates, not by handler-side guesswork or by relying on
///   registration order:
///   - `TextUndo`/`TextRedo` are scoped `Some("text-input")` - one shared context tag carried by
///     every real text-typing surface in the app and by nothing else: `crate::palette`'s query
///     panel, `crate::rail`'s filter row, `crate::settings`' Keybindings filter row,
///     `crate::root::new_file`'s name prompt, `crate::code_surface::render`'s code surface (only
///     while the editable File view is genuinely showing, alongside its existing `"file-editor"`
///     tag), and `crate::merge::editing`'s hand-edit surface (alongside `"merge-editor"`).
///   - `Undo`/`Redo` gain the matching `&& !text-input`, so the two predicate sets are provably
///     disjoint: no live context stack can satisfy both. That is deliberately *not* left to
///     GPUI's tie-break rules. `vendor/zed/crates/gpui/src/keymap.rs`'s own `bindings_for_input`
///     orders equally-deep matches by registration index, and `KeyBindingContextPredicate::
///     depth_of` (`.../keymap/context.rs:260`) reports the *same* depth for `"text-input"` and
///     `"!terminal"` when a text surface is the deepest focused node - so with only the old
///     predicate, which of the two undo systems ran would have come down to the order of two
///     lines in this function. This project has shipped the "a keystroke gets swallowed or goes
///     to the wrong handler" bug class seven-plus times (documented throughout this very list);
///     an ordering-dependent answer to "does Ctrl+Z undo my typing or my commit" is not a risk
///     worth taking.
///   - The terminal is unaffected in both directions: no terminal surface ever carries
///     `"text-input"` (a real terminal wants `Ctrl+Z` as the literal `SIGTSTP` byte - see the
///     `Undo`/`Redo` entry above), so neither system fires there and the keystroke stays free to
///     reach the pty, exactly as before.
///   - `"ctrl-y"` is bound to `TextRedo` as well, per GitHub issue #17's own checklist, and is
///     deliberately a literal `Ctrl` on every OS (like `"ctrl-shift-t"` above, unlike this list's
///     usual `"secondary-"`): `Ctrl+Y` is the Windows-convention redo key, and `Cmd+Y` means
///     nothing on macOS. Safe to bind only because `"text-input"` is never live over a terminal,
///     where `Ctrl+Y` is a real control byte (`0x19`, readline `yank`).
///   - Routing between the six text surfaces is *not* done by inspecting app state inside one
///     handler: each surface registers its own `on_action` listener on the exact node that carries
///     its `"text-input"` tag, and GPUI only dispatches an action along the focused node's own
///     ancestor path (`vendor/zed/crates/gpui/src/window.rs`'s `dispatch_action_on_node`), so the
///     focused widget's handler is the only one that can run. This matters for a real, reachable
///     case a state-inspecting handler would get wrong: the command palette can be open with a
///     typed query while a file editor is still open behind it, and Ctrl+Z must undo the query.
pub fn default_key_bindings() -> Vec<gpui::KeyBinding> {
    vec![
        gpui::KeyBinding::new("secondary-n", root::NewSession, None),
        gpui::KeyBinding::new("secondary-k", root::TogglePalette, None),
        gpui::KeyBinding::new("secondary-,", root::ToggleSettings, None),
        gpui::KeyBinding::new("secondary-z", root::Undo, Some("!terminal && !text-input")),
        gpui::KeyBinding::new(
            "secondary-shift-z",
            root::Redo,
            Some("!terminal && !text-input"),
        ),
        gpui::KeyBinding::new("secondary-z", root::TextUndo, Some("text-input")),
        gpui::KeyBinding::new("secondary-shift-z", root::TextRedo, Some("text-input")),
        gpui::KeyBinding::new("ctrl-y", root::TextRedo, Some("text-input")),
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
        // `crate::code_surface::render::AdeApp::render_code_surface`'s own docs for exactly where
        // that context tag comes from, and `crate::lsp::completion_popup::AdeApp::
        // completions_open_for_active_path`'s own docs for why `Loading`/`Failed` don't count).
        // `Tab` has no competing plain-`Editor*` *action* binding anywhere in this list - but the
        // real, live-verified reason scoping it only to `"file-editor && completions"` is safe
        // isn't "nothing else claims it": a real, live keystroke test confirms that with the
        // popup closed, an unbound `tab` keystroke still reaches the real edit buffer and inserts
        // a literal `\t`, the same way any other unbound printable character does - GPUI falls
        // through to the platform's ordinary text-input/IME path (`crate::code_surface::editing`'s
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
        // `crate::merge::editing`) - a distinct `"merge-editor"` context, deliberately never
        // `"file-editor"` itself: the same real `Editor*` action *types*/handler bodies are
        // reused (see `crate::code_surface::editing::AdeApp::active_edit_target`'s own docs for how a
        // handler routes to whichever buffer is actually the current target), but reusing
        // `"file-editor"` verbatim would also pull in the `"... && completions"`/
        // `"... && !completions"` narrowing above, which makes no sense for a merge buffer - no
        // completions popup is ever wired up for it (see `crate::merge::editing`'s own top
        // docs). `EditorSaveAnyway` (`secondary-shift-s`) is deliberately *not* bound here either:
        // there is no external-change-conflict concept for a merge hand-edit buffer (see
        // `crate::merge::flow::AdeApp::save_merge_edit`'s own docs).
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
                    // The design's title-bar band (`crate::title_bar`) draws its own
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

/// GitHub issue #17's scoping matrix, proven as predicate logic against every real key-context
/// stack this app actually produces - not as a dispatch-order observation.
///
/// This is deliberately *stronger* than the `simulate_keystrokes` routing tests in
/// `crate::root::focus::text_undo_scoping_tests` and `crate::code_surface::editing::editing_tests`,
/// and complements them rather than duplicating them. GPUI dispatches only the highest-precedence
/// matching binding (`vendor/zed/crates/gpui/src/keymap.rs`'s `bindings_for_input`, then
/// `window.rs`'s `replay_pending_input`, which stops at the first handler that doesn't propagate),
/// so a live keystroke test can only ever observe the *winner* - it would happily pass even if
/// both undo systems matched and the right one merely happened to be registered second. This
/// asserts the thing that actually matters: for every real context stack, **at most one** of the
/// two systems is enabled at all, so nothing about this depends on the order of two lines in
/// [`default_key_bindings`]. See that function's own docs for the full rationale, and this
/// project's documented seven-plus instances of the "keystroke reaches the wrong handler" bug
/// class for why it is checked this precisely.
#[cfg(test)]
mod undo_scoping_matrix_tests {
    use gpui::{KeyBinding, KeyContext};

    /// The real context stacks this matrix runs over, taken from
    /// [`crate::keymap_overrides::real_context_stacks`] rather than restated here - one source of
    /// truth, guarded against drift by that module's own `every_real_key_context_call_site_is_covered`
    /// test, so this matrix and the Settings rebind collision checker can never disagree about the
    /// app they are both reasoning over.
    ///
    /// Includes the **empty** stack. An earlier version of this matrix restated its own list and
    /// omitted it while claiming to cover "every real context stack this app can produce" - which
    /// was exactly wrong in the place it mattered, since the empty stack is what GPUI falls back to
    /// when the focused `FocusId` is not in the last rendered frame, and an independent adversarial
    /// audit found a real, reachable dangling-focus site (Settings page navigation) living there.
    /// The matrix's "at most one system is live" invariant held vacuously on that stack while the
    /// keystroke was in fact silently swallowed. It is now asserted explicitly, with its own honest
    /// meaning: neither system live, which is a real bug class this app fixes at the focus sites
    /// rather than in the keymap.
    fn real_context_stacks() -> Vec<Vec<&'static str>> {
        crate::keymap_overrides::real_context_stacks()
    }

    /// A human-readable label per stack, index-aligned with [`real_context_stacks`].
    fn stack_descriptions() -> Vec<&'static str> {
        vec![
            "a dangling focus handle - GPUI's empty-context dispatch-root fallback",
            "any surface with no key context of its own (the Settings overlay, the file tree, \
             the tab strip) - only the root div's baseline tag",
            "a focused terminal session",
            "the read-only Diff view",
            "the editable File view",
            "the editable File view with the completions popup open",
            "the merge hand-edit surface",
            "a focused single-line text input (palette / rail filter / settings filter / \
             new-file prompt)",
        ]
    }

    fn stack(parts: &[&str]) -> Vec<KeyContext> {
        parts
            .iter()
            .map(|part| KeyContext::parse(part).expect("a real, parseable key context"))
            .collect()
    }

    fn enabled(binding: &KeyBinding, contexts: &[KeyContext]) -> bool {
        match binding.predicate() {
            Some(predicate) => predicate.depth_of(contexts).is_some(),
            None => true,
        }
    }

    fn bindings_for(action: &str, keystroke: &str) -> Vec<KeyBinding> {
        crate::default_key_bindings()
            .into_iter()
            .filter(|binding| {
                binding.action().name() == action
                    && binding.keystrokes().len() == 1
                    && binding.keystrokes()[0].inner().unparse() == keystroke
            })
            .collect()
    }

    /// A real `secondary-z` keystroke can never be claimed by both undo systems at once, in any
    /// real context - and is claimed by the *expected* one in each.
    #[test]
    fn secondary_z_is_claimed_by_at_most_one_undo_system_in_every_real_context() {
        let keystroke = if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        };
        let worktree = bindings_for("app::Undo", keystroke);
        let text = bindings_for("app::TextUndo", keystroke);
        assert_eq!(worktree.len(), 1, "one real worktree-level Undo binding");
        assert_eq!(text.len(), 1, "one real text-undo binding");

        // Index-aligned with `real_context_stacks()`/`stack_descriptions()`.
        let expectations: Vec<(bool, bool)> = vec![
            // A dangling focus handle: GPUI evaluates every predicate against an empty stack and
            // `eval_inner` short-circuits to false, so *neither* system is live and the keystroke
            // is silently swallowed. Asserted, not tolerated: this is a real, reachable bug class
            // (four sites fixed on this branch), and the fix belongs at the focus sites - a
            // keymap that "handled" an empty stack would be handling a frame that isn't on screen.
            (false, false),
            (true, false),  // anything with no key context of its own
            (false, false), // a focused terminal - the pty gets the real SIGTSTP byte
            (true, false),  // the read-only Diff view - nothing to text-undo there
            (false, true),  // the editable File view
            (false, true),  // ...with completions open
            (false, true),  // the merge hand-edit surface
            (false, true),  // a focused single-line text input
        ];
        assert_eq!(expectations.len(), real_context_stacks().len());

        for ((description, parts), (wants_worktree, wants_text)) in stack_descriptions()
            .into_iter()
            .zip(real_context_stacks())
            .zip(expectations)
        {
            let contexts = stack(&parts);
            let worktree_enabled = enabled(&worktree[0], &contexts);
            let text_enabled = enabled(&text[0], &contexts);
            assert!(
                !(worktree_enabled && text_enabled),
                "both undo systems are live for {description} - which one actually runs would \
                 then come down to registration order, the exact fragility \
                 crate::default_key_bindings' `!text-input` narrowing exists to remove"
            );
            assert_eq!(
                worktree_enabled, wants_worktree,
                "worktree-level Undo enablement for {description}"
            );
            assert_eq!(
                text_enabled, wants_text,
                "text undo enablement for {description}"
            );
        }
    }

    /// The same matrix for both real redo spellings.
    #[test]
    fn every_redo_spelling_is_claimed_by_at_most_one_undo_system_in_every_real_context() {
        let shift_z = if cfg!(target_os = "macos") {
            "cmd-shift-z"
        } else {
            "ctrl-shift-z"
        };
        let worktree = bindings_for("app::Redo", shift_z);
        assert_eq!(worktree.len(), 1);
        let text: Vec<KeyBinding> = crate::default_key_bindings()
            .into_iter()
            .filter(|binding| binding.action().name() == "app::TextRedo")
            .collect();
        assert_eq!(
            text.len(),
            2,
            "TextRedo is bound twice by design: secondary-shift-z and ctrl-y"
        );

        for (description, parts) in stack_descriptions().into_iter().zip(real_context_stacks()) {
            let contexts = stack(&parts);
            let worktree_enabled = enabled(&worktree[0], &contexts);
            for binding in &text {
                assert!(
                    !(worktree_enabled && enabled(binding, &contexts)),
                    "both redo systems are live for {description}"
                );
            }
        }
    }

    /// `ctrl-y` must be genuinely dead over a terminal: it is a real control byte there
    /// (`0x19`, readline `yank`), and this app binds it globally to nothing.
    #[test]
    fn ctrl_y_is_never_live_over_a_focused_terminal() {
        let contexts = stack(&["app", "terminal"]);
        for binding in crate::default_key_bindings() {
            if binding.keystrokes().len() == 1
                && binding.keystrokes()[0].inner().unparse() == "ctrl-y"
            {
                assert!(
                    !enabled(&binding, &contexts),
                    "{} must not claim ctrl-y while a terminal has focus",
                    binding.action().name()
                );
            }
        }
    }
}
