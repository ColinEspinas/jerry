//! `app`: the ADE desktop application shell.
//!
//! A three-pane GPUI window: a left sidebar listing the target repository's real git
//! worktrees (via `wt-core`) with agent/tab controls for spawning agent CLIs or shells
//! into them, a tabbed center pane of real terminal agents (via `pty-core` +
//! `alacritty_terminal`), and a right sidebar showing the active worktree's real file tree
//! (via `std::fs::read_dir`). See `crate::root`, `crate::work_surface::agents`, `crate::terminal::pane`,
//! and `crate::terminal::grid` for the interesting design decisions (entity/state model,
//! blocking-call offloading, terminal grid rendering).

pub mod code_surface;
pub mod env_info;
pub mod fonts;
pub mod graph_view;
pub mod icon_pack;
pub mod keymap;
pub mod keymap_overrides;
pub mod language;
pub mod lsp;
pub mod merge;
pub mod palette;
// `pub(crate)`: a single small lock shared by `rail::repo`/`sidebar::fold_state`/
// `work_surface::tab_order_state`'s own `save_merged_at` methods - see its own module docs for
// the real intra-process concurrent-writer hazard (GitHub issue #90's "New Window") this exists
// to close. Nothing outside this crate has any business calling it directly.
pub(crate) mod persisted_state_lock;
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
// GitHub issue #87: real update-available detection + click-to-update. `pub(crate)`, matching
// `title_bar`'s own precedent just above (see that module's own comment): nothing outside this
// crate needs `crate::updater` directly, only `crate::root`/`crate::status_bar`/
// `crate::palette`'s own wiring into it.
pub(crate) mod updater;
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
/// terminal agent).
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
/// - The `+` menu's "Open file…" row has **no** global keybinding of its own, despite the
///   mockup's own `mod+P` spec: `"secondary-p"` is now claimed by `TogglePalette` (below) instead
///   of this narrower action - see that entry's own docs for the real terminal-readline tradeoff
///   that decision accepts. The `+` menu row itself is still a working, click-only way to open
///   the palette scoped to files.
/// - `"]"` (Next changed file) has no modifier, and is scoped to `Some("diff && !file-editor")`
///   rather than global - the only one of this app's bindings with a non-`'rail'`/`'agent'`
///   context. A global `"]"` would swallow a literal `]` typed into any focused terminal/agent
///   agent (closing a bracket, an array literal, a regex class) - the same bug class as above.
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
/// - `"secondary-1"` through `"secondary-8"` back the tab strip's agent-jump keycaps
///   (`root::AdeApp::jump_to_agent_at`), expanding the design's `mod+1…8` spec into eight
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
///   would swallow ordinary typing in every focused terminal agent the same way an unscoped
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
/// - `TextUndo`/`TextRedo` (GitHub issue #17, `crate::text_history`) back this app's one real
///   undo system: per-widget **text** undo. `"secondary-z"`/`"secondary-shift-z"` follow this
///   list's own `"secondary-"` convention, but - unlike every other entry above except `"]"` -
///   are **not** globally scoped (`None`): `"secondary-z"` resolves to plain `Ctrl+Z` on
///   Linux/Windows, which `crate::terminal::pane::keystroke_to_bytes` already maps to the real
///   `SIGTSTP` control byte (`0x1a`) - the terminal-suspend keystroke essentially every
///   interactive terminal program relies on. A global binding here would swallow it before it
///   ever reached a focused terminal's own key handling, the same "app-level shortcut steals
///   terminal input" bug class this list's own `secondary-p`/`"]"` docs already cover, just for a
///   far more disruptive keystroke to silently lose than either of those.
///   - `TextUndo`/`TextRedo` are scoped `Some("text-input")` - one shared context tag carried by
///     every real text-typing surface in the app and by nothing else: `crate::palette`'s query
///     panel, `crate::rail`'s filter row, `crate::settings`' Keybindings filter row,
///     `crate::root::new_file`'s name prompt, `crate::code_surface::render`'s code surface (only
///     while the editable File view is genuinely showing, alongside its existing `"file-editor"`
///     tag), `crate::merge::editing`'s hand-edit surface (alongside `"merge-editor"`), and
///     `crate::sidebar::render`'s file tree (only while its inline New File / New Folder / Rename
///     name editor is open, alongside `"file-tree tree-editing"` - see
///     `crate::keymap_overrides::file_tree_key_context`). That last one arrived by *merge* rather
///     than by an edit to this file: GitHub issue #19 built those editors on a branch where this
///     tag did not exist, and until they gained it `Ctrl+Z` mid-filename ran the worktree-level
///     undo/redo this app used to also have (removed - GitHub issue #47). Keeping this
///     enumeration complete is exactly what that incident argues for.
///   - No terminal surface ever carries `"text-input"` (a real terminal wants `Ctrl+Z` as the
///     literal `SIGTSTP` byte), so `TextUndo`/`TextRedo` never fire there and the keystroke stays
///     free to reach the pty, exactly as before.
///   - `"ctrl-y"` is bound to `TextRedo` as well, per GitHub issue #17's own checklist, and is
///     deliberately a literal `Ctrl` on every OS (like `"ctrl-shift-t"` above, unlike this list's
///     usual `"secondary-"`): `Ctrl+Y` is the Windows-convention redo key, and `Cmd+Y` means
///     nothing on macOS. Safe to bind only because `"text-input"` is never live over a terminal,
///     where `Ctrl+Y` is a real control byte (`0x19`, readline `yank`).
///   - Routing between the seven text surfaces is *not* done by inspecting app state inside one
///     handler: each surface registers its own `on_action` listener on the exact node that carries
///     its `"text-input"` tag, and GPUI only dispatches an action along the focused node's own
///     ancestor path (`vendor/zed/crates/gpui/src/window.rs`'s `dispatch_action_on_node`), so the
///     focused widget's handler is the only one that can run. This matters for a real, reachable
///     case a state-inspecting handler would get wrong: the command palette can be open with a
///     typed query while a file editor is still open behind it, and Ctrl+Z must undo the query.
/// - GitHub issue #26 added four more real entries, each documented at its own binding below:
///   `"ctrl-space"` (`CompletionsInvoke`, manual completion trigger/refresh), `"tab"`/`"shift-tab"`
///   (`EditorIndent`/`EditorDedent`, real Tab/Shift+Tab indentation, replacing the old "falls
///   through to plain-text-insertion" behavior `Tab` used to have with the popup closed),
///   `"escape"` (the real accessibility escape hatch `Tab`'s new meaning inside the editor
///   requires - `EditorCollapseCursors` in the File view, since GitHub issue #28's own multi-
///   cursor collapse and this escape hatch both want the File view's plain `Escape` and only one
///   binding can genuinely own it at equal context depth; a separate `EditorEscape` in the merge
///   hand-edit view, which never gets multi-cursor actions and so never faces that collision -
///   see `crate::code_surface::editing::AdeApp::handle_editor_collapse_cursors_action`'s own docs
///   for exactly how the File view composes both real behaviors from one binding), and `"ctrl-w"`
///   (`CloseFocusedTab`, close the focused tab) - the last of these follows the exact same
///   literal-`ctrl`/`!terminal`-scoping precedent `"ctrl-shift-t"` above already established, for
///   the same real reasons. `EditorIndent`/`EditorDedent` are scoped
///   `"file-editor && !completions"`/`"merge-editor"` like every other plain `Editor*` action,
///   never `"text-input"` - that tag is GitHub issue #17's own **text-history** context, a
///   different real concept from "is real text editing allowed here" (see the `TextUndo`/
///   `TextRedo` entry above), and `Tab`/`Shift+Tab`/`Escape` have no meaning at all in this app's
///   five non-editor text-input surfaces (a filter row, a query field, a rename prompt) that
///   would need claiming there.
pub fn default_key_bindings() -> Vec<gpui::KeyBinding> {
    vec![
        gpui::KeyBinding::new("secondary-n", root::NewAgent, None),
        // The palette's real shortcut, matching the VS Code/Sublime "command palette" convention
        // directly rather than reusing "secondary-k". "secondary-k" was the original binding, but
        // the file-editor's real `"ctrl-k ctrl-d"` chord (multi-cursor "skip occurrence",
        // Revision R13) registers `"ctrl-k"` as a real chord *prefix* in that context - a lone
        // Ctrl+K there now waits through gpui's real ~1s prefix-timeout (`Window`'s own
        // `pending_input` timer) before replaying as a plain keystroke and reaching
        // `TogglePalette`, a real, noticeable delay a second, unambiguous shortcut avoids
        // entirely. Deliberately unscoped (not `!terminal`) and a real, accepted replacement, not
        // an alias alongside "secondary-k": a focused terminal's own `Ctrl+P` (readline
        // `previous-history`, `0x10`) is now shadowed by this binding rather than reaching the
        // shell - an explicit, known tradeoff, not an oversight (a terminal's own Up-arrow history
        // navigation is unaffected either way).
        gpui::KeyBinding::new("secondary-p", root::TogglePalette, None),
        gpui::KeyBinding::new("secondary-,", root::ToggleSettings, None),
        gpui::KeyBinding::new("secondary-z", root::TextUndo, Some("text-input")),
        gpui::KeyBinding::new("secondary-shift-z", root::TextRedo, Some("text-input")),
        gpui::KeyBinding::new("ctrl-y", root::TextRedo, Some("text-input")),
        gpui::KeyBinding::new("f12", root::GotoDefinition, None),
        gpui::KeyBinding::new("ctrl-shift-t", root::NewTerminal, None),
        gpui::KeyBinding::new("secondary-shift-n", root::NewAgentPane, None),
        gpui::KeyBinding::new("secondary-shift-g", root::NewGitGraph, None),
        gpui::KeyBinding::new("]", root::NextChangedFile, Some("diff && !file-editor")),
        gpui::KeyBinding::new("secondary-1", root::JumpToAgent1, None),
        gpui::KeyBinding::new("secondary-2", root::JumpToAgent2, None),
        gpui::KeyBinding::new("secondary-3", root::JumpToAgent3, None),
        gpui::KeyBinding::new("secondary-4", root::JumpToAgent4, None),
        gpui::KeyBinding::new("secondary-5", root::JumpToAgent5, None),
        gpui::KeyBinding::new("secondary-6", root::JumpToAgent6, None),
        gpui::KeyBinding::new("secondary-7", root::JumpToAgent7, None),
        gpui::KeyBinding::new("secondary-8", root::JumpToAgent8, None),
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
        // GitHub issue #27's "Ctrl+Shift+arrows (word-wise)" - `secondary-*` is this same
        // codebase's own established platform-aware Ctrl/Cmd alias (`secondary-a`/`secondary-c`/
        // etc. immediately below), not a new convention.
        gpui::KeyBinding::new("secondary-left", root::EditorWordLeft, Some("file-editor")),
        gpui::KeyBinding::new(
            "secondary-right",
            root::EditorWordRight,
            Some("file-editor"),
        ),
        gpui::KeyBinding::new(
            "secondary-shift-left",
            root::EditorSelectWordLeft,
            Some("file-editor"),
        ),
        gpui::KeyBinding::new(
            "secondary-shift-right",
            root::EditorSelectWordRight,
            Some("file-editor"),
        ),
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
        // Multi-cursor (Revision R13, issue #28) - `crate::code_surface::edit_buffer`'s own
        // "Multi-cursor" docs for the overall design; each handler's own docs
        // (`crate::code_surface::editing::AdeApp::handle_editor_select_next_occurrence_action` and
        // its three siblings) for exactly what each keystroke does. File-view-only, like the rest
        // of this list's plain `Editor*` entries - `crate::merge::editing`'s `"merge-editor"`
        // context deliberately does not get these (see `crate::code_surface::render::AdeApp::
        // render_code_surface`'s own docs for why). `"ctrl-d"`/`"ctrl-shift-l"` are deliberately
        // literal `ctrl-`, not `"secondary-"`, matching the mockup-independent, cross-editor VS
        // Code convention issue #28 itself names (`Ctrl+D`/`Ctrl+Shift+L`/`Ctrl+K Ctrl+D` are the
        // same physical keys on every OS in VS Code itself, including macOS, unlike this list's
        // other `secondary-` bindings which intentionally follow the *platform* modifier
        // instead) - `"secondary-d"` would also collide with `EditorSelectAllOccurrences`'s own
        // real `Cmd+Shift+L`-independent behavior on macOS anyway (VS Code never rebinds `Ctrl+D`
        // to `Cmd+D` there either; `Cmd+D` is already the macOS-only system-wide "bookmark this
        // page" shortcut in every browser, a real, live conflict `"ctrl-d"` avoids by staying
        // literal). `"ctrl-k ctrl-d"` is a real, supported space-separated chord binding (verified
        // against the pinned `gpui` dependency's own `crates/gpui/src/keymap/binding.rs`, which
        // splits a keybinding's own keystroke string on whitespace into an ordered chord sequence
        // - not invented here), matching VS Code's own two-keystroke default for "skip current,
        // find next".
        gpui::KeyBinding::new(
            "ctrl-d",
            root::EditorSelectNextOccurrence,
            Some("file-editor"),
        ),
        gpui::KeyBinding::new(
            "ctrl-shift-l",
            root::EditorSelectAllOccurrences,
            Some("file-editor"),
        ),
        gpui::KeyBinding::new(
            "ctrl-k ctrl-d",
            root::EditorSkipOccurrence,
            Some("file-editor"),
        ),
        // Real Tab/Shift+Tab indentation (GitHub issue #26) - scoped `"file-editor &&
        // !completions"`, the same narrowing `enter`/`up`/`down` above already use, so a genuinely
        // open Completions popup still gets `Tab`/`Escape` for `CompletionsAccept`/
        // `CompletionsDismiss` instead (see the `Completions*` group below).
        gpui::KeyBinding::new(
            "tab",
            root::EditorIndent,
            Some("file-editor && !completions"),
        ),
        gpui::KeyBinding::new(
            "shift-tab",
            root::EditorDedent,
            Some("file-editor && !completions"),
        ),
        // `Escape` in the File view is `EditorCollapseCursors`, not a separate `EditorEscape`
        // binding - GPUI resolves two same-keystroke bindings at equal context depth by load
        // order (later wins), confirmed against the pinned `gpui` dependency's own
        // `key_dispatch.rs` test suite, so registering both here would silently shadow whichever
        // loaded first rather than run both. `EditorCollapseCursors`'s own handler (`crate::
        // code_surface::editing::AdeApp::handle_editor_collapse_cursors_action`) already composes
        // both real behaviors: a genuine multi-cursor collapse when one is active, or GitHub issue
        // #26's accessibility escape-hatch fallback (`Self::escape_focus_off_editor`) for the
        // exact same "nothing multi-cursor-related to do" case it was already documented as a
        // no-op for - since `Tab`/`Shift+Tab` are now real indent/dedent actions while the editor
        // has focus, a keyboard-only user still needs some way to leave it and keep tabbing
        // through the rest of the UI once that fallback fires.
        gpui::KeyBinding::new(
            "escape",
            root::EditorCollapseCursors,
            Some("file-editor && !completions"),
        ),
        // Real Completions popup navigation/accept/dismiss (Revision R8.5b) - scoped to
        // `"file-editor && completions"`, the real *narrower* mirror of the `!completions`
        // narrowing on `enter`/`up`/`down` above, added to the same code-surface node only while
        // `AdeApp::completions` is genuinely, actionably `Ready` for the active file (see
        // `crate::code_surface::render::AdeApp::render_code_surface`'s own docs for exactly where
        // that context tag comes from, and `crate::lsp::completion_popup::AdeApp::
        // completions_open_for_active_path`'s own docs for why `Loading`/`Failed` don't count).
        // `Tab` here is `CompletionsAccept`, scoped `"file-editor && completions"` - GitHub issue
        // #26 gave plain `tab`/`shift-tab` (with the popup closed) their own real
        // `EditorIndent`/`EditorDedent` bindings further up, in the `"file-editor && !completions"`
        // group alongside `enter`/`up`/`down` - see those bindings' own docs for why `Tab` is no
        // longer left to fall through to GPUI's ordinary text-input path the way this comment used
        // to describe.
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
        // `Ctrl+Space` (GitHub issue #26) - opens/refreshes the Completions popup on demand at
        // the caret, scoped to plain `"file-editor"` (not narrowed to `"... && completions"` or
        // `"... && !completions"`, unlike every binding above): it must fire identically whether
        // the popup is already open (a real in-place refresh - see `crate::lsp::completion_popup::
        // AdeApp::handle_completions_invoke_action`'s own docs) or closed (opening it fresh).
        // Deliberately a literal `"ctrl-space"`, not `"secondary-space"`, matching this list's own
        // `"ctrl-shift-t"` precedent for a keystroke that must stay the same physical key on every
        // OS - Ctrl+Space is the universal convention for "trigger completion" across IDEs/
        // editors, and the design's Linux caveat (Ctrl+Space is also a common IME-switch chord on
        // some Linux desktops) is exactly why this binding, like every other one in this list, is
        // fully rebindable from the Settings > Keybindings page (`crate::keymap_overrides`) rather
        // than hardcoded - see that module's own docs for the real override mechanism this and
        // every other binding here already gets for free just by being registered here.
        gpui::KeyBinding::new("ctrl-space", root::CompletionsInvoke, Some("file-editor")),
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
        gpui::KeyBinding::new("secondary-left", root::EditorWordLeft, Some("merge-editor")),
        gpui::KeyBinding::new(
            "secondary-right",
            root::EditorWordRight,
            Some("merge-editor"),
        ),
        gpui::KeyBinding::new(
            "secondary-shift-left",
            root::EditorSelectWordLeft,
            Some("merge-editor"),
        ),
        gpui::KeyBinding::new(
            "secondary-shift-right",
            root::EditorSelectWordRight,
            Some("merge-editor"),
        ),
        gpui::KeyBinding::new("home", root::EditorHome, Some("merge-editor")),
        gpui::KeyBinding::new("end", root::EditorEnd, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-a", root::EditorSelectAll, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-c", root::EditorCopy, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-x", root::EditorCut, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-v", root::EditorPaste, Some("merge-editor")),
        gpui::KeyBinding::new("secondary-s", root::EditorSave, Some("merge-editor")),
        // Real Tab/Shift+Tab indentation plus the Escape focus-out hatch (GitHub issue #26),
        // mirrored here for the same real reason every other `Editor*` action is: the merge hand-
        // edit buffer is a real, independent `EditBuffer` too (see `crate::code_surface::editing::
        // AdeApp::active_edit_target`'s own docs for how the shared `handle_editor_indent_action`/
        // `handle_editor_dedent_action`/`handle_editor_escape_action` handlers route to it).
        gpui::KeyBinding::new("tab", root::EditorIndent, Some("merge-editor")),
        gpui::KeyBinding::new("shift-tab", root::EditorDedent, Some("merge-editor")),
        gpui::KeyBinding::new("escape", root::EditorEscape, Some("merge-editor")),
        // GitHub issue #19's file-tree bindings. Every one is scoped to
        // `"file-tree && !tree-editing && !tree-delete-confirm"`, and all three terms are
        // load-bearing - see `crate::sidebar::tree_ops`'s own module docs for the full
        // reasoning. The short version: `secondary-c`/`secondary-x`/`secondary-v` at `None`
        // scope would claim the control bytes `crate::terminal::pane::keystroke_to_bytes` hands
        // a focused shell (Ctrl+C is SIGINT - binding it globally would risk making it
        // impossible to interrupt a running agent CLI), the exact bug class this list's `"]"`
        // and `secondary-z` entries above already document (`secondary-p` is a deliberate
        // *exception* to it, not a precedent for avoiding it - see that binding's own docs);
        // `!tree-editing` is what keeps them
        // from firing while one of the tree's own inline name editors has the keystroke; and
        // `!tree-delete-confirm` the same while the modal delete confirmation is up (`F2` or
        // `Shift+F10` firing *behind* a modal scrim was a real finding in this change's own
        // review). Both negated terms follow the `"file-editor && !completions"` shape above.
        //
        // `shift-f10` is the only context-menu keystroke bound. The dedicated Menu/Application
        // key that some keyboards also carry is deliberately *not* bound: gpui's key names come
        // from each platform backend at runtime and nothing in the vendored tree names that key
        // (it isn't in `vendor/zed/crates/gpui/src/platform/keystroke.rs`'s own key vocabulary),
        // so any spelling guessed here would be a binding that silently never matches.
        gpui::KeyBinding::new(
            "shift-f10",
            root::FileTreeContextMenu,
            Some("file-tree && !tree-editing && !tree-delete-confirm"),
        ),
        gpui::KeyBinding::new(
            "f2",
            root::FileTreeRename,
            Some("file-tree && !tree-editing && !tree-delete-confirm"),
        ),
        gpui::KeyBinding::new(
            "secondary-c",
            root::FileTreeCopy,
            Some("file-tree && !tree-editing && !tree-delete-confirm"),
        ),
        gpui::KeyBinding::new(
            "secondary-x",
            root::FileTreeCut,
            Some("file-tree && !tree-editing && !tree-delete-confirm"),
        ),
        gpui::KeyBinding::new(
            "secondary-v",
            root::FileTreePaste,
            Some("file-tree && !tree-editing && !tree-delete-confirm"),
        ),
        // `Ctrl+W` (GitHub issue #26) - closes the focused tab (file or agent), via
        // `crate::work_surface::render::AdeApp::handle_close_focused_tab_action`'s own docs for
        // exactly what "focused" resolves to and why this can never close the window (this app
        // registers no window-close keybinding anywhere in this list, on any platform, so there
        // is no native default here to intercept in the first place). Scoped `Some("!terminal")`,
        // the same real precedent `"ctrl-shift-t"` above already established for this exact
        // conflict class: plain `Ctrl+W` is `crate::terminal::pane::keystroke_to_bytes`'s own real
        // `unix-word-rerase` control byte (`0x17`), a standard readline word-backspace a focused
        // shell needs unclaimed - a global binding here would swallow it in every focused
        // terminal/agent on Linux/Windows, the same bug class this list's own
        // `"]"`/`secondary-z` entries already document (`secondary-p` deliberately accepts this
        // exact class of collision instead - see that binding's own docs for why). A
        // terminal/agent tab is still always closeable via the tab strip's own `×` or
        // middle-click regardless.
        gpui::KeyBinding::new("ctrl-w", root::CloseFocusedTab, Some("!terminal")),
    ]
}

/// The [`WindowOptions`] every ADE window opens with - both the one real window [`run`] itself
/// opens at process startup, and every window `crate::title_bar::menu`'s "New Window" row opens
/// later (GitHub issue #90) - one shared literal so the two can never silently drift apart, the
/// same "no second copy of a decision" reasoning [`default_key_bindings`]'s own docs give.
/// `Bounds::centered` needs a real `&App` to read the display it's centering against
/// (`vendor/zed/crates/gpui/src/geometry.rs:740`), which both real call sites have (the launch
/// callback's own `&mut App`, and a menu row's `Context<AdeApp>`, which derefs to `App` -
/// `vendor/zed/crates/gpui/src/app/context.rs:25-37`).
pub(crate) fn default_window_options(cx: &App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(1440.0), px(928.0)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // The design's title-bar band (`crate::title_bar`) draws its own close/minimize/maximize
        // controls, so the OS/compositor shouldn't also draw a native titlebar - matching
        // `vendor/zed/crates/zed/src/zed.rs`'s own `titlebar`/`window_decorations` combination.
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
    }
}

/// Opens the ADE window against `repo_path` and runs the GPUI event loop until the window is
/// closed. Blocks the calling thread for the application's lifetime, mirroring
/// `gpui::Application::run`'s own contract (`vendor/zed/crates/gpui/examples/hello_world.rs`).
///
/// GitHub issue #90: `repo_path` is now optional. `Some(path)` behaves exactly as before (the
/// real CLI-argument case, `crate::main`'s own docs). `None` - a fresh launch with no CLI
/// argument - is handed straight through to `root::AdeApp::new`/`new_with_settings`, which
/// resolves it against whatever repo (if any) was last focused and persisted to `repos.toml`:
/// reopen that one, or - if nothing was ever persisted, or the remembered path no longer exists -
/// a genuinely empty window with no folder open at all. This is the real fix for "launching the
/// editor should be in an 'empty' state by default": there is no `env::current_dir()` fallback
/// anywhere in this path anymore (removed from `crate::main` itself) for `None` to silently
/// become.
pub fn run(repo_path: Option<PathBuf>) {
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

            let options = default_window_options(cx);
            let opened = cx.open_window(options, {
                let repo_path = repo_path.clone();
                // `true`: the real process-launch path, which is exactly the one case GitHub
                // issue #90 wants a remembered folder auto-reopened for - unlike a "New Window"
                // menu row later in the same running process (`crate::title_bar::menu`'s own
                // handler), which deliberately passes `false` for a genuinely empty window
                // regardless of what's persisted. See `root::AdeApp::new`'s own docs for exactly
                // what this flag controls.
                move |window, cx| {
                    cx.new(|cx| root::AdeApp::new(repo_path.clone(), true, window, cx))
                }
            });

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
/// so a live keystroke test can only ever observe the *winner*, which would happily pass even if
/// `TextUndo`/`TextRedo` were live somewhere this list's own docs don't claim. This asserts the
/// thing that actually matters: for every real context stack, `TextUndo`/`TextRedo` are enabled
/// in exactly the contexts `Some("text-input")` is documented to mean, and nowhere else. See
/// [`default_key_bindings`]'s own docs for the full rationale, and this project's documented
/// seven-plus instances of the "keystroke reaches the wrong handler" bug class for why it is
/// checked this precisely.
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
    /// `TextUndo`/`TextRedo` are disabled on that stack, which is asserted explicitly, with its
    /// own honest meaning: the keystroke is silently swallowed, a real bug class this app fixes at
    /// the focus sites rather than in the keymap.
    fn real_context_stacks() -> Vec<Vec<&'static str>> {
        crate::keymap_overrides::real_context_stacks()
    }

    /// A human-readable label per stack, index-aligned with [`real_context_stacks`].
    fn stack_descriptions() -> Vec<&'static str> {
        vec![
            "a dangling focus handle - GPUI's empty-context dispatch-root fallback",
            "any surface with no key context of its own (the Settings overlay, the tab strip) - \
             only the root div's baseline tag",
            "a focused terminal agent",
            "the read-only Diff view",
            "the editable File view",
            "the editable File view with the completions popup open",
            "the merge hand-edit surface",
            "a focused single-line text input (palette / rail filter / settings filter / \
             new-file prompt)",
            "the focused file tree, no overlay open",
            "the file tree's inline name editor (new file / new folder / rename)",
            "the file tree's modal delete confirmation",
            "the file tree's inline name editor with the delete confirmation over it",
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

    /// `TextUndo` is enabled in exactly the real contexts `Some("text-input")` is documented to
    /// mean, and nowhere else.
    #[test]
    fn secondary_z_reaches_text_undo_in_exactly_the_real_text_input_contexts() {
        let keystroke = if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        };
        let text = bindings_for("app::TextUndo", keystroke);
        assert_eq!(text.len(), 1, "one real text-undo binding");

        // Index-aligned with `real_context_stacks()`/`stack_descriptions()`.
        let expectations: Vec<bool> = vec![
            // A dangling focus handle: GPUI evaluates every predicate against an empty stack and
            // `eval_inner` short-circuits to false, so the keystroke is silently swallowed.
            // Asserted, not tolerated: this is a real, reachable bug class (four sites fixed on
            // this branch), and the fix belongs at the focus sites - a keymap that "handled" an
            // empty stack would be handling a frame that isn't on screen.
            false, false, // anything with no key context of its own
            false, // a focused terminal - the pty gets the real SIGTSTP byte
            false, // the read-only Diff view - nothing to text-undo there
            true,  // the editable File view
            true,  // ...with completions open
            true,  // the merge hand-edit surface
            true,  // a focused single-line text input
            // The file tree with no editor open is not a text surface.
            false,
            // ...but its inline name editor *is* one. This row is the whole reason issue #19's
            // tree had to gain issue #17's `"text-input"` tag when the two branches merged:
            // without it Ctrl+Z while typing a filename used to reach the worktree-level undo
            // this app used to also have (removed - GitHub issue #47), with the tree's own key
            // handler never even seeing the keystroke (it returns early on a
            // `control`/`platform` modifier).
            true, false, // the modal delete confirmation - no text being typed
            true,  // the name editor, delete confirmation over it
        ];
        // A stack added without a matching expectation would otherwise silently drop a row from
        // this matrix rather than fail it (`zip` truncates to the shortest input).
        assert_eq!(expectations.len(), real_context_stacks().len());
        assert_eq!(stack_descriptions().len(), real_context_stacks().len());

        for ((description, parts), wants_text) in stack_descriptions()
            .into_iter()
            .zip(real_context_stacks())
            .zip(expectations)
        {
            let contexts = stack(&parts);
            let text_enabled = enabled(&text[0], &contexts);
            assert_eq!(
                text_enabled, wants_text,
                "text undo enablement for {description}"
            );
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
