//! Real, persisted keybinding overrides layered on top of `crate::default_key_bindings()` - the
//! Settings > Keybindings page's rebind mechanism (see
//! `crate::root::AdeApp::render_settings_keymap_page`'s own docs for the render/interaction
//! side). This module is the GPUI-typed core: building the *effective* keybinding set from
//! defaults plus overrides, and checking whether a candidate new keystroke would collide with
//! another binding that could realistically be active at the same time. Deliberately its own
//! module rather than folded into `crate::keymap` (which stays GPUI-free by design - see that
//! module's own docs) or `crate::settings::state` (which only *reads* already-registered
//! `gpui::KeyBinding`s, never constructs new ones).
//!
//! None of this needs a live `gpui::Window`/`App` - `gpui::KeyBinding`, `gpui::Keystroke`, and
//! `gpui::KeyBindingContextPredicate` are all plain data types once built, so every function here
//! is a real `#[test]`, not a `#[gpui::test]`.
//!
//! ## Identity: how one override maps to one default binding
//!
//! `crate::default_key_bindings()` has no stable id per entry - two different bindings can share
//! the same action (`CompletionsAccept` is bound twice, to `tab` and to `enter`) or the same
//! command label across disjoint contexts (`Editor::copy` under `"file-editor"` and again under
//! `"merge-editor"`). [`BindingIdentity`] is the real, unique key this module uses instead: the
//! action's real [`gpui::Action::name`], its real registered context predicate's `Display` string
//! (`"global"` when `None`, matching `crate::settings::state::KeybindingRow::context`'s own convention),
//! and the real *default* keystroke(s) it originally shipped with, joined the same
//! space-separated way `gpui::KeyBinding::load` itself splits a multi-keystroke chord string
//! (each part `gpui::Keystroke::unparse()`'d). Two default bindings can only collide under this
//! identity if they share all three - command, context, *and* default keystroke - which would
//! already make them indistinguishable rows on the Keybindings page itself
//! (`crate::settings::state::tests::every_keybinding_row_is_genuinely_distinguishable_from_every_other`
//! guards against exactly that in the default set).
//!
//! ## Collision detection is exact for this app, by enumeration
//!
//! `vendor/zed/crates/gpui/src/keymap/context.rs`'s `KeyBindingContextPredicate` has no built-in
//! "could these two predicates ever both be true for some real context stack" check - only
//! `eval`/`depth_of` (against one concrete, already-known stack) and `is_superset` (whether every
//! context the other predicate matches, this one also matches). Rather than approximate the
//! missing check, [`contexts_could_overlap`] **evaluates both predicates against every context
//! stack this app really produces** ([`real_context_stacks`]), through GPUI's own `depth_of` -
//! the same function `Keymap::binding_enabled` uses to decide whether a binding is live. Two
//! predicates "could overlap" exactly when some real stack makes both live. That is not a
//! heuristic: it is exact over this app's real state space, and it needs no `Or`/`Not` solver
//! because it never reasons about predicate *structure* at all.
//!
//! It replaced an `is_superset`-plus-special-cases heuristic that GitHub issue #17 made newly and
//! dangerously wrong, found by an independent adversarial audit. `is_superset` answers `false` in
//! *both* directions for two different bare `Identifier`s, and the old code read that absence of
//! evidence as proof of disjointness. That was survivable only while distinct identifiers really
//! did live on distinct nodes; issue #17 put `"text-input"` on the *same* node as `"file-editor"`
//! and `"merge-editor"`, so `"text-input"` vs `"file-editor"` reported "disjoint" when they are in
//! fact always live together - and a user could rebind `Text: undo` onto Backspace, Ctrl+V or
//! Escape through the real Settings UI with no warning at all.
//!
//! The one place it is deliberately conservative is coverage of its own enumeration: a predicate
//! that is live on **no** stack in [`real_context_stacks`] reports "could overlap" rather than
//! certifying a disjointness this module evidently cannot see. That is a strictly safe direction
//! for a rebind guard - an extra warning on the Keybindings page, never a missed collision - and
//! it earned its keep at the merge that brought GitHub issue #19's file tree into this branch:
//! the tree's five bindings were live on no enumerated stack, so every one of them reported
//! "could overlap" until the four `file-tree*` stacks were added, which is what surfaced the
//! omission instead of letting it pass silently.

use gpui::{KeyBinding, KeyBindingContextPredicate, Keystroke};

use crate::settings::store::KeybindingOverride;

/// The real, stable identity of one default keybinding - see the module docs' "Identity" section.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BindingIdentity {
    pub action: String,
    pub context: String,
    pub default_keystrokes: String,
}

impl BindingIdentity {
    /// Derives the real identity of an already-registered `KeyBinding` - used both to identify
    /// which row a "record new shortcut" click is for, and, internally, to match `overrides`
    /// entries against `crate::default_key_bindings()` in [`effective_key_bindings`].
    pub fn of(binding: &KeyBinding) -> Self {
        BindingIdentity {
            action: binding.action().name().to_string(),
            context: context_label(binding.predicate().as_deref()),
            default_keystrokes: join_keystrokes(binding),
        }
    }

    /// Whether `override_entry` is the real, persisted override for this identity - the three
    /// fields compared are exactly [`KeybindingOverride`]'s own identity fields, which mirror
    /// this struct's fields one-for-one by construction (see that type's own docs).
    pub fn matches_override(&self, override_entry: &KeybindingOverride) -> bool {
        self.action == override_entry.action
            && self.context == override_entry.context
            && self.default_keystrokes == override_entry.default_keystrokes
    }
}

/// `"global"` for an unscoped binding, else the predicate's real `Display` string - the same
/// convention `crate::settings::state::KeybindingRow::context` already uses, so a `BindingIdentity`'s
/// `context` field always matches what the Keybindings page itself shows for that row.
fn context_label(predicate: Option<&KeyBindingContextPredicate>) -> String {
    match predicate {
        Some(predicate) => predicate.to_string(),
        None => "global".to_string(),
    }
}

fn join_keystrokes(binding: &KeyBinding) -> String {
    binding
        .keystrokes()
        .iter()
        .map(|keystroke| keystroke.inner().unparse())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds the real, effective keybinding set: `crate::default_key_bindings()`, with every
/// binding whose [`BindingIdentity`] matches a real, persisted `overrides` entry replaced by a
/// freshly built `gpui::KeyBinding` carrying the *same* action and context but the override's own
/// keystroke(s). Uses `gpui::KeyBinding::load` (not the generic `KeyBinding::new`, which needs a
/// concrete, compile-time-known `Action` type) since it accepts an already-boxed `dyn Action`
/// plus an already-parsed context predicate - exactly what a real registered `KeyBinding` already
/// carries via its own `action()`/`predicate()` accessors
/// (`vendor/zed/crates/gpui/src/keymap/binding.rs`). A malformed override keystroke (only
/// reachable via a hand-edited `settings.toml` - the real rebind UI only ever writes a value it
/// just parsed successfully itself) is skipped, leaving the default binding in place, rather than
/// panicking the whole app at startup or silently dropping every other real override in the same
/// load.
pub fn effective_key_bindings(overrides: &[KeybindingOverride]) -> Vec<KeyBinding> {
    crate::default_key_bindings()
        .into_iter()
        .map(|binding| {
            let identity = BindingIdentity::of(&binding);
            let Some(override_entry) = overrides
                .iter()
                .find(|entry| identity.matches_override(entry))
            else {
                return binding;
            };
            match KeyBinding::load(
                &override_entry.keystrokes,
                binding.action().boxed_clone(),
                binding.predicate(),
                false,
                None,
                &gpui::DummyKeyboardMapper,
            ) {
                Ok(rebuilt) => rebuilt,
                Err(_) => binding,
            }
        })
        .collect()
}

/// Every real key-context stack this app's own render code can actually produce, as the
/// space-separated context literals each node contributes, ordered root-first exactly the way
/// GPUI's own `dispatch_path` builds them.
///
/// This is the single source of truth for "what contexts really exist", shared by
/// [`contexts_could_overlap`] and by `crate::undo_scoping_matrix_tests`, so the two can never
/// disagree about the app they are both reasoning over. Derived by hand from the fourteen
/// `.key_context(..)` call sites in the crate - `crate::root::AdeApp::render`'s baseline `"app"`,
/// `crate::terminal::pane`, `crate::code_surface::render` (one call site, three literals from a
/// `match`), `crate::merge::editing`, the eight single-line inputs (the rail's agent filter,
/// Settings' keymap filter, the palette's own query field, `root::new_file`'s inline name editor,
/// `crate::graph_view::render`'s Branches filter box, the Themes page's "Generate from colour"
/// seed field, the General page's "Shell" field (GitHub issue #213), and - newest, GitHub issue
/// #241 - the git graph row menu's "Create branch here" prompt), plus GitHub issue #242 phase B's
/// own new single-line input (`crate::graph_view::rebase_render`'s interactive-rebase plan row
/// reword field), which all emit the same bare `"text-input"`, and
/// `crate::sidebar::render::AdeApp::file_tree_shell` (one call site, four literals from a
/// `match`) - and guarded against drift by this module's own
/// `every_real_key_context_call_site_is_covered` test, which fails the moment a fifteenth call
/// site appears. (That test earned its keep immediately: this comment originally said "six",
/// counting distinct emitted literals rather than real call sites, and the test caught it on its
/// first run; it has since caught this comment drifting behind four more real inputs.)
///
/// The four `file-tree*` stacks arrived with a merge, not with an edit to this file, which is
/// exactly why they are called out here. GitHub issue #19's file tree and issue #17's text-undo
/// scoping were built on separate branches; this list was issue #17's and knew nothing about the
/// tree. Omitting them was not a silent hole in the *checker* - `contexts_could_overlap` refuses
/// to certify a predicate it never sees live and returned "could overlap" for all five file-tree
/// bindings, which is what surfaced this at merge time. It was a real hole in every *positive*
/// claim built on the list, including `crate::undo_scoping_matrix_tests`' "at most one undo
/// system is live anywhere", which held vacuously over the tree's inline name editor.
///
/// The empty stack is deliberately included: GPUI falls back to the dispatch root with **no**
/// context frames whenever the focused `FocusId` isn't in the last rendered frame, and
/// `KeyBindingContextPredicate::eval_inner` short-circuits to `false` for an empty stack
/// (`vendor/zed/crates/gpui/src/keymap/context.rs`), so *every* scoped binding is dead there. That
/// is a real, reachable state (a dangling focus handle) and leaving it out of the enumeration
/// would quietly make every claim built on this list unsound for exactly the case that has bitten
/// this project repeatedly.
pub fn real_context_stacks() -> Vec<Vec<&'static str>> {
    vec![
        vec![],
        vec!["app"],
        vec!["app", "terminal"],
        vec!["app", "diff"],
        vec!["app", "diff file-editor text-input"],
        vec!["app", "diff file-editor text-input completions"],
        vec!["app", "merge-editor text-input"],
        vec!["app", "text-input"],
        // The interactive-rebase plan surface (GitHub issue #304), and the same surface with one
        // of its own `reword` rows' message fields focused - a real `"text-input"` node nested
        // *inside* the `"rebase-plan"` container, which is the whole reason that surface's
        // plain-letter bindings carry `&& !text-input` (see `crate::default_key_bindings`).
        vec!["app", "rebase-plan"],
        vec!["app", "rebase-plan", "text-input"],
        // The diff pane's review-notes surface (GitHub issue #288), and the same surface with one
        // of its pinned note cards open for typing - a real `"text-input"` node nested *inside*
        // the `"diff-view"` container, exactly like `"rebase-plan"` above and for the same
        // reason: it is why `ToggleLineNote`'s plain `c` carries `&& !text-input` while
        // `SendReviewNotes`' `mod+enter` deliberately does not (see
        // `crate::default_key_bindings`).
        //
        // Note the `"diff"` frame ahead of both: the notes container is a *descendant* of
        // `crate::code_surface::render`'s own node, so `"diff"`-scoped bindings are live over a
        // focused note card. Getting this wrong here is not academic - it is exactly what hid the
        // live bug where `v` (`ToggleChangeSeen`) and `]` (`NextChangedFile`) swallowed those
        // characters instead of letting them be typed into a note.
        vec!["app", "diff", "diff-view"],
        vec!["app", "diff", "diff-view", "text-input"],
        // GitHub issue #162 adds two more bare `"text-input"` frames, and their *stacks* are the
        // interesting part rather than the tag:
        //
        // - the Search panel's four fields live in the right sidebar, which is not inside any
        //   other keyed container, so they are the plain `vec!["app", "text-input"]` already
        //   listed above - no new stack.
        // - the in-file find bar is deliberately a **sibling** of the code surface's own
        //   `"diff file-editor text-input"` node rather than a child of it (see
        //   `crate::code_surface::render::AdeApp::render_code_surface`), which is exactly why it
        //   is *also* the plain `vec!["app", "text-input"]` and not a
        //   `["app", "diff file-editor text-input", "text-input"]`. Nested, every arrow key,
        //   Enter and Esc typed into the bar would have matched an `Editor*` binding instead -
        //   a real, reproduced bug, not a hypothetical.
        //
        // So neither adds a stack here; both add a call site, which is what the drift guard
        // below counts.
        //
        // The file tree (GitHub issue #19), built by calling the *same* function the renderer
        // calls, over both of its inputs - not by restating its literals here. See
        // [`file_tree_key_context`] for why that indirection is load-bearing rather than tidy.
        vec!["app", file_tree_key_context(false)],
        vec!["app", file_tree_key_context(true)],
    ]
}

/// The file tree's real key context, as a function of its one remaining modal state: whether an
/// inline name editor is open. (GitHub issue #105 removed the other one, the delete
/// confirmation - delete now runs immediately, with no modal state of its own to add a context
/// word for.)
///
/// This lives here, beside [`real_context_stacks`], rather than inline in
/// `crate::sidebar::render::AdeApp::file_tree_shell` where it is used, because the two must never
/// disagree and "a `match` in the renderer plus a hand-copied list of its literals here" is
/// exactly how they would. That is not hypothetical: the merge that brought GitHub issue #19's
/// tree onto issue #17's branch added a `.key_context(..)` call site whose literals this
/// enumeration knew nothing about, and the drift test written to catch precisely that was blind
/// to it. Deriving both from one function makes that class of drift structurally impossible
/// rather than merely test-detectable - a guard that has to be extended by hand every time the
/// thing it guards grows is not a guard.
///
/// `"text-input"` is emitted for the editor-open case: an inline name editor is a real
/// text-typing surface, and the tag is what routes `Ctrl+Z` mid-filename to `TextUndo` rather
/// than `FileTreeUndo`.
pub fn file_tree_key_context(inline_edit: bool) -> &'static str {
    if inline_edit {
        "file-tree tree-editing text-input"
    } else {
        "file-tree"
    }
}

fn parsed_context_stacks() -> Vec<Vec<gpui::KeyContext>> {
    real_context_stacks()
        .into_iter()
        .map(|stack| {
            stack
                .into_iter()
                .map(|part| {
                    gpui::KeyContext::parse(part).expect("a real, parseable key context literal")
                })
                .collect()
        })
        .collect()
}

/// `true` when a binding scoped to `a` and one scoped to `b` could realistically both be active
/// for the same live context stack, so giving them the same keystroke would leave GPUI's own
/// dispatch order (not this app) to decide which one actually fires.
///
/// Decided by **evaluating both predicates against every stack this app really produces**
/// ([`real_context_stacks`]) through GPUI's own `depth_of` - the exact same function
/// `Keymap::binding_enabled` uses to decide whether a binding is live
/// (`vendor/zed/crates/gpui/src/keymap.rs`). Exact for this app, rather than a heuristic.
///
/// It replaced a `is_superset`-based approximation that GitHub issue #17 made newly, dangerously
/// wrong, found by an independent adversarial audit. `is_superset`
/// (`vendor/zed/crates/gpui/src/keymap/context.rs`) evaluates `false` in *both* directions for two
/// different bare `Identifier`s, and the old code read that absence of evidence as proof of
/// disjointness. That was survivable while distinct identifiers really did live on distinct nodes
/// (but this issue put `"text-input"` on the *same* node as `"file-editor"` and `"merge-editor"`,
/// so `"text-input"` vs `"file-editor"` reported "disjoint" when they are in fact always live
/// together). A user could rebind `Text: undo` onto Backspace, Ctrl+V or Escape through the real
/// Settings UI with no warning at all, and the rebind would then silently lose to the editor's own
/// binding on dispatch-order grounds. This module's own tests now pin exactly that case.
///
/// Conservative where it doesn't understand something: a predicate that isn't live on any real
/// stack at all reports "could overlap" rather than clearing a risk this function can't actually
/// reason about.
fn contexts_could_overlap(
    a: Option<&KeyBindingContextPredicate>,
    b: Option<&KeyBindingContextPredicate>,
) -> bool {
    let (a, b) = match (a, b) {
        // An unscoped (global) binding is active in every non-empty context - always a real
        // overlap with anything else that can fire at all.
        (None, _) | (_, None) => return true,
        (Some(a), Some(b)) => (a, b),
    };
    if a == b {
        return true;
    }
    let stacks = parsed_context_stacks();
    let live = |predicate: &KeyBindingContextPredicate| {
        stacks
            .iter()
            .filter(|stack| predicate.depth_of(stack).is_some())
            .count()
    };
    if live(a) == 0 || live(b) == 0 {
        // One of them matches nothing this function knows about - don't claim disjointness on the
        // strength of an enumeration that evidently doesn't cover it.
        return true;
    }
    stacks
        .iter()
        .any(|stack| a.depth_of(stack).is_some() && b.depth_of(stack).is_some())
}

/// Checks whether `candidate_keystroke` (a `gpui::Keystroke::parse`-compatible string, exactly
/// the form a real captured chord is serialized to before this is called) would collide with any
/// *other* binding in `effective` that could realistically share a live context with
/// `for_binding` - see the module docs.
///
/// `defaults` and `effective` must be the same real, index-aligned pair
/// `crate::default_key_bindings()`/`effective_key_bindings(overrides)` always produces (the
/// latter is a straight `.map()` over the former, one output per input, in order) - self-exclusion
/// compares `for_binding`'s identity against each **default** binding's identity at the same
/// index, not against `effective[i]`'s own identity. This is deliberate, not incidental: if the
/// row being rebound already carries a real override, `effective[i]` for that same row is the
/// *already-rebuilt* binding, whose own `BindingIdentity` reports its current (overridden)
/// keystroke as `default_keystrokes` - comparing that against `for_identity` (derived from the
/// real, original default binding) would never match, so a second rebind of an already-overridden
/// row would falsely report a collision against its own, about-to-be-replaced binding. Comparing
/// against `defaults[i]` instead is stable across any number of rebinds.
///
/// Returns the first real colliding binding, if any, or `None` if `candidate_keystroke` is
/// genuinely safe to bind - including when it fails to parse at all (an invalid chord can't
/// collide with anything; the caller surfaces the parse failure itself, not this function).
pub fn find_colliding_binding<'a>(
    defaults: &[KeyBinding],
    effective: &'a [KeyBinding],
    for_binding: &KeyBinding,
    candidate_keystroke: &str,
) -> Option<&'a KeyBinding> {
    let candidate = Keystroke::parse(candidate_keystroke).ok()?;
    let for_identity = BindingIdentity::of(for_binding);
    let for_predicate = for_binding.predicate();
    defaults
        .iter()
        .zip(effective.iter())
        .find_map(|(default_binding, effective_binding)| {
            if BindingIdentity::of(default_binding) == for_identity {
                return None;
            }
            let single_keystroke_matches = effective_binding.keystrokes().len() == 1
                && effective_binding.keystrokes()[0].inner() == &candidate;
            if !single_keystroke_matches {
                return None;
            }
            contexts_could_overlap(
                for_predicate.as_deref(),
                effective_binding.predicate().as_deref(),
            )
            .then_some(effective_binding)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn override_for(binding: &KeyBinding, new_keystrokes: &str) -> KeybindingOverride {
        let identity = BindingIdentity::of(binding);
        KeybindingOverride {
            action: identity.action,
            context: identity.context,
            default_keystrokes: identity.default_keystrokes,
            keystrokes: new_keystrokes.to_string(),
        }
    }

    #[test]
    fn effective_key_bindings_with_no_overrides_matches_the_real_defaults_exactly() {
        let defaults = crate::default_key_bindings();
        let effective = effective_key_bindings(&[]);
        assert_eq!(defaults.len(), effective.len());
        for (default, actual) in defaults.iter().zip(effective.iter()) {
            assert_eq!(BindingIdentity::of(default), BindingIdentity::of(actual));
        }
    }

    #[test]
    fn a_real_override_replaces_only_the_matching_default_binding_keystrokes() {
        let defaults = crate::default_key_bindings();
        let palette_binding = defaults
            .iter()
            .find(|b| b.action().name() == "app::TogglePalette")
            .expect("TogglePalette should be a real default binding");
        let override_entry = override_for(palette_binding, "ctrl-shift-p");

        let effective = effective_key_bindings(std::slice::from_ref(&override_entry));

        let rebuilt = effective
            .iter()
            .find(|b| b.action().name() == "app::TogglePalette")
            .expect("the overridden binding must still exist under the same action");
        assert_eq!(
            rebuilt.keystrokes()[0].inner(),
            &Keystroke::parse("ctrl-shift-p").unwrap(),
            "the override's own keystroke must be the one that's actually registered"
        );

        // Every other binding must be completely untouched.
        for (default, actual) in defaults.iter().zip(effective.iter()) {
            if default.action().name() == "app::TogglePalette" {
                continue;
            }
            assert_eq!(BindingIdentity::of(default), BindingIdentity::of(actual));
        }
    }

    #[test]
    fn a_malformed_persisted_override_keystroke_is_skipped_not_a_crash() {
        let defaults = crate::default_key_bindings();
        let palette_binding = defaults
            .iter()
            .find(|b| b.action().name() == "app::TogglePalette")
            .expect("TogglePalette should be a real default binding");
        // Three plain, non-modifier dash-separated segments - `gpui::Keystroke::parse` only ever
        // accepts a single trailing key after its recognized modifier prefixes, so this is a
        // real, guaranteed `Err`, not just an unusual-looking string.
        let override_entry = override_for(palette_binding, "one-two-three");
        assert!(
            Keystroke::parse(&override_entry.keystrokes).is_err(),
            "sanity check: this test's whole premise depends on this string genuinely failing \
             to parse"
        );

        let effective = effective_key_bindings(std::slice::from_ref(&override_entry));

        // No panic reaching this line is the real assertion; the default must still be intact.
        let untouched = effective
            .iter()
            .find(|b| b.action().name() == "app::TogglePalette")
            .expect("TogglePalette must still be bound");
        assert_eq!(
            BindingIdentity::of(untouched),
            BindingIdentity::of(palette_binding),
            "a malformed override must leave the real default binding in place"
        );
    }

    #[test]
    fn two_disjoint_contexts_do_not_collide_even_with_the_same_keystroke() {
        // Real, live example from `crate::default_key_bindings`'s own registered set:
        // `EditorCopy` is bound to `secondary-c` under both `"file-editor"` and `"merge-editor"`
        // - two genuinely mutually-exclusive contexts (a file tab and a merge hand-edit buffer
        // are never both the focused surface at once).
        let bindings = crate::default_key_bindings();
        let file_editor_copy = bindings
            .iter()
            .find(|b| {
                b.action().name() == "app::EditorCopy"
                    && context_label(b.predicate().as_deref()) == "file-editor"
            })
            .expect("a file-editor-scoped EditorCopy binding should exist");
        let merge_editor_copy = bindings
            .iter()
            .find(|b| {
                b.action().name() == "app::EditorCopy"
                    && context_label(b.predicate().as_deref()) == "merge-editor"
            })
            .expect("a merge-editor-scoped EditorCopy binding should exist");
        assert_ne!(
            BindingIdentity::of(file_editor_copy),
            BindingIdentity::of(merge_editor_copy),
            "these must be genuinely distinct identities for this test to prove anything"
        );

        // Rebinding the file-editor one to the merge-editor one's own real keystroke must not
        // report a collision - the two scopes can never both be live at once.
        let collision = find_colliding_binding(
            &bindings,
            &bindings,
            file_editor_copy,
            &merge_editor_copy.keystrokes()[0].inner().unparse(),
        );
        assert!(
            collision.is_none(),
            "file-editor and merge-editor are genuinely disjoint contexts - no real collision"
        );
    }

    #[test]
    fn a_narrower_context_colliding_with_its_own_broader_parent_context_is_flagged() {
        // `"file-editor"` (broad) vs `"file-editor && completions"` (narrower, a real subset) -
        // `contexts_could_overlap` must catch this via `is_superset`, unlike the genuinely
        // disjoint case above. `escape` is real-world-shared by two scoped bindings under
        // `"file-editor"` since GitHub issue #26 added a real Tab/Shift+Tab-driven escape hatch
        // scoped `"file-editor && !completions"`, alongside the pre-existing `CompletionsDismiss`
        // (`"file-editor && completions"`) - it's `EditorCollapseCursors` that actually owns that
        // exact keystroke+context today, not a separate `EditorEscape` action: GitHub issue #28's
        // own multi-cursor collapse and this escape hatch both want the File view's plain
        // `Escape`, and only one binding can genuinely own it at equal context depth (see
        // `crate::code_surface::editing::AdeApp::handle_editor_collapse_cursors_action`'s own
        // docs for how it composes both real behaviors from this one binding).
        // `find_colliding_binding` returns the *first* match in real registration order
        // (`crate::default_key_bindings`), which is `EditorCollapseCursors` (registered before
        // the `Completions*` group) - either real match would equally prove the point this test
        // exists for (a broad `"file-editor"` rebind collides with *some* real narrower
        // `"file-editor && ..."` binding sharing the same keystroke), so asserting on the specific
        // one `find_colliding_binding` actually returns keeps this test honest about its own real
        // behavior rather than papering over which one - matching e.g. `"up"`, which both
        // `EditorUp` *and* `CompletionsUp` already use under their own, different (but still
        // overlapping) contexts.
        let bindings = crate::default_key_bindings();
        let editor_left = bindings
            .iter()
            .find(|b| {
                b.action().name() == "app::EditorLeft"
                    && context_label(b.predicate().as_deref()) == "file-editor"
            })
            .expect("a file-editor-scoped EditorLeft binding should exist");
        let editor_collapse_cursors = bindings
            .iter()
            .find(|b| {
                b.action().name() == "app::EditorCollapseCursors"
                    && context_label(b.predicate().as_deref()) == "file-editor && !completions"
            })
            .expect("a file-editor-scoped EditorCollapseCursors binding should exist");

        let collision = find_colliding_binding(
            &bindings,
            &bindings,
            editor_left,
            &editor_collapse_cursors.keystrokes()[0].inner().unparse(),
        );
        assert_eq!(
            collision.map(|b| b.action().name()),
            Some("app::EditorCollapseCursors"),
            "file-editor is a real superset of file-editor && !completions - rebinding \
             EditorLeft onto EditorCollapseCursors's own keystroke must be flagged"
        );
    }

    #[test]
    fn colliding_with_a_global_binding_is_always_flagged_regardless_of_the_other_scope() {
        let bindings = crate::default_key_bindings();
        let editor_left = bindings
            .iter()
            .find(|b| {
                b.action().name() == "app::EditorLeft"
                    && context_label(b.predicate().as_deref()) == "file-editor"
            })
            .expect("a file-editor-scoped EditorLeft binding should exist");
        let new_agent = bindings
            .iter()
            .find(|b| b.action().name() == "app::NewAgent")
            .expect("NewAgent should be a real global default binding");
        assert_eq!(context_label(new_agent.predicate().as_deref()), "global");

        let collision = find_colliding_binding(
            &bindings,
            &bindings,
            editor_left,
            &new_agent.keystrokes()[0].inner().unparse(),
        );
        assert_eq!(collision.map(|b| b.action().name()), Some("app::NewAgent"));
    }

    #[test]
    fn a_binding_never_collides_with_its_own_current_keystroke() {
        let bindings = crate::default_key_bindings();
        let palette_binding = bindings
            .iter()
            .find(|b| b.action().name() == "app::TogglePalette")
            .expect("TogglePalette should be a real default binding");
        let own_keystroke = palette_binding.keystrokes()[0].inner().unparse();

        let collision =
            find_colliding_binding(&bindings, &bindings, palette_binding, &own_keystroke);
        assert!(
            collision.is_none(),
            "checking a binding against its own already-registered keystroke must never report \
             a self-collision"
        );
    }

    /// The exact reason GitHub issue #17 needed this checker made precise: `"file-editor"` and
    /// `"text-input"` are emitted by the **same node**, so a `file-editor`-scoped binding rebound
    /// onto `secondary-z` really does collide - with `TextUndo`.
    #[test]
    fn a_file_editor_binding_rebound_onto_secondary_z_collides_with_text_undo() {
        let bindings = crate::default_key_bindings();
        let editor_copy = bindings
            .iter()
            .find(|b| {
                b.action().name() == "app::EditorCopy"
                    && context_label(b.predicate().as_deref()) == "file-editor"
            })
            .expect("a file-editor-scoped EditorCopy binding should exist");
        let text_undo = bindings
            .iter()
            .find(|b| b.action().name() == "app::TextUndo")
            .expect("TextUndo should be a real default binding");

        let collision = find_colliding_binding(
            &bindings,
            &bindings,
            editor_copy,
            &text_undo.keystrokes()[0].inner().unparse(),
        );
        assert_eq!(
            collision.map(|b| b.action().name()),
            Some("app::TextUndo"),
            "the binding actually co-resident on the same node is the one that must be named"
        );
    }

    /// The audit's own reproduction, pinned: `"text-input"` and `"file-editor"` live on the same
    /// node, so rebinding `Text: undo` onto a key the editor already claims must warn. The
    /// heuristic this replaced reported no collision at all for every one of these, letting a user
    /// silently rebind text undo onto Backspace, Ctrl+V or Escape through the real Settings UI and
    /// then find it did nothing in the editor.
    #[test]
    fn rebinding_text_undo_onto_a_key_the_editor_already_claims_is_flagged() {
        let bindings = crate::default_key_bindings();
        let text_undo = bindings
            .iter()
            .find(|b| b.action().name() == "app::TextUndo")
            .expect("TextUndo should be a real default binding");

        for (keystroke, expected) in [
            ("backspace", "app::EditorBackspace"),
            // `escape` is real-world-shared by two scoped bindings under `"file-editor"` since
            // GitHub issue #26 added a real escape hatch (`"file-editor && !completions"`,
            // via `EditorCollapseCursors` - see that binding's own docs for why it's not a
            // separate `EditorEscape` action here) alongside the pre-existing `CompletionsDismiss`
            // (`"file-editor && completions"`) - either is a real collision that proves this
            // test's own point; `find_colliding_binding` finds `EditorCollapseCursors` first
            // since it's registered earlier in `crate::default_key_bindings`.
            ("escape", "app::EditorCollapseCursors"),
        ] {
            let collision = find_colliding_binding(&bindings, &bindings, text_undo, keystroke);
            assert_eq!(
                collision.map(|b| b.action().name()),
                Some(expected),
                "rebinding Text: undo onto {keystroke} must be flagged"
            );
        }
        // `secondary-v` resolves per-OS exactly the way `EditorPaste`'s own binding does.
        let paste = bindings
            .iter()
            .find(|b| {
                b.action().name() == "app::EditorPaste"
                    && context_label(b.predicate().as_deref()) == "file-editor"
            })
            .expect("a file-editor-scoped EditorPaste binding should exist");
        let collision = find_colliding_binding(
            &bindings,
            &bindings,
            text_undo,
            &paste.keystrokes()[0].inner().unparse(),
        );
        assert!(
            collision.is_some(),
            "rebinding Text: undo onto the editor's own paste key must be flagged too"
        );
    }

    /// Counts real `.key_context(..)` call sites in **every** `.rs` file under this crate's
    /// `src/`, by reading the real directory at test time.
    ///
    /// The hand-listed file set this replaced is why a merge could add a ninth call site with the
    /// drift test still green: it named eight files and `include_str!`'d each one, and GitHub
    /// issue #19's file tree put its `.key_context(..)` in `sidebar/render.rs`, which was not
    /// among them - so the sum stayed at 8 and the guard built to catch "a new call site
    /// appeared" reported success. `include_str!` needs literal paths, so the file set could not
    /// be globbed at compile time; `std::fs` at test time can be.
    ///
    /// Counts occurrences within a line rather than only lines that *start* with the call, so a
    /// chained `div().id("x").key_context("y")` is not invisible to it. Lines whose first
    /// non-space characters are `//` are skipped, which is a real (if coarse) filter now that the
    /// match is no longer anchored - it does not understand block comments or string literals,
    /// and does not need to: over-counting fails loudly and safely.
    fn key_context_call_sites() -> Vec<(String, usize)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, usize)>, root: &std::path::Path) {
            let entries = std::fs::read_dir(dir).expect("this crate's own src/ must be readable");
            let mut paths: Vec<std::path::PathBuf> =
                entries.map(|e| e.expect("dir entry").path()).collect();
            // Sorted so a failure message is stable run to run.
            paths.sort();
            for path in paths {
                if path.is_dir() {
                    walk(&path, out, root);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    // This module itself is skipped, and only this one: it holds the search
                    // pattern as a literal and names it in assertion messages, so it would
                    // self-match three times. It contains no renderer and therefore no real
                    // `.key_context(..)` call - those live in the `render`/`pane` modules this
                    // still walks in full. Excluding it by name rather than by trying to parse
                    // string literals out of arbitrary Rust keeps the walker honest about what
                    // it does and does not understand.
                    if path.file_name().is_some_and(|n| n == "keymap_overrides.rs") {
                        continue;
                    }
                    let text = std::fs::read_to_string(&path).expect("a readable .rs file");
                    let count: usize = text
                        .lines()
                        .filter(|line| !line.trim_start().starts_with("//"))
                        .map(|line| line.matches(".key_context(").count())
                        .sum();
                    if count > 0 {
                        let name = path
                            .strip_prefix(root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned();
                        out.push((name, count));
                    }
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        walk(&root, &mut out, &root);
        out
    }

    /// Drift guard for [`real_context_stacks`], which the collision checker's exactness depends
    /// on entirely: that list is hand-derived from this crate's `.key_context(..)` call sites, so
    /// a new call site appearing without a matching entry would silently make every disjointness
    /// answer unsound. Reads the real source rather than trusting a comment.
    #[test]
    fn every_real_key_context_call_site_is_covered() {
        let sites = key_context_call_sites();
        let call_sites: usize = sites.iter().map(|(_, count)| count).sum();
        assert_eq!(
            call_sites, 21,
            "real_context_stacks() is hand-derived from exactly twenty-one .key_context(..) call \
             sites (fourteen of which emit the same bare \"text-input\": the palette, the rail \
             filter, the new-file prompt, the Branches filter, the Keybindings filter, the \
             Themes page's \"Generate from colour\" seed input, the General page's \"Shell\" \
             field (GitHub issue #213), GitHub issue #241's git graph row menu's \"Create \
             branch here\" prompt, GitHub issue #242 phase B's interactive-rebase plan row's own \
             reword message field, GitHub issue #285's Changes panel commit message field, and - \
             GitHub issue #288's pinned review-note card, and - GitHub issue #162's \
             Search panel row and its in-file find bar, and - newest - GitHub issue #401's \
             Editor > Search page's \"add a pattern\" row - plus GitHub issue #304's own \
             \"rebase-plan\" and issue #288's own \"diff-view\") - a new one means \
             that list, and every disjointness answer built on \
             it, needs updating. Real sites found: {sites:?}"
        );

        // The file tree's own site is named explicitly: it is the one a merge added while this
        // test was still passing, because the old hand-listed file set didn't include it.
        assert!(
            sites
                .iter()
                .any(|(name, _)| name.replace('\\', "/") == "sidebar/render.rs"),
            "the file tree's own .key_context(..) must be among the scanned sites - it is the \
             call site a whitelist-based version of this test was structurally blind to. \
             Found: {sites:?}"
        );

        // Every literal any of those sites can emit must appear verbatim in the enumeration.
        //
        // The file tree's two are *derived* from `file_tree_key_context`, which is also what the
        // renderer calls - so for the tree this loop is now a tautology, and deliberately so:
        // the guarantee moved out of the test and into the structure, where drift is impossible
        // rather than merely detectable. Stated plainly because a tautological assertion that
        // looked like a real one would be worse than none.
        //
        // The remaining literals are still hand-listed against `match`es inlined in their own
        // render functions, and are honestly weaker for it: `code_surface/render.rs`'s three-arm
        // one could gain a fourth arm and only the hand-edit of this array would catch it. Giving
        // it the same treatment is the real fix and is not done here.
        let known: Vec<&str> = real_context_stacks().into_iter().flatten().collect();
        let tree_literals: Vec<&str> = [false, true]
            .into_iter()
            .map(file_tree_key_context)
            .collect();
        for literal in [
            "app",
            "terminal",
            "diff",
            "diff file-editor text-input",
            "diff file-editor text-input completions",
            "merge-editor text-input",
            "text-input",
            "rebase-plan",
        ]
        .into_iter()
        .chain(tree_literals)
        {
            assert!(
                known.contains(&literal),
                "context literal {literal:?} is emitted by real render code but missing from \
                 real_context_stacks()"
            );
        }
    }

    /// The negated-conjunct scan must still be conservative in the *other* direction: a
    /// `"!terminal && !text-input"` binding and a plain `"diff"` one really can both be live (the
    /// read-only Diff view is neither a terminal nor a text input), so that pair must still flag.
    #[test]
    fn a_conjunction_of_negations_still_flags_a_scope_it_cannot_rule_out() {
        let undo = KeyBindingContextPredicate::parse("!terminal && !text-input")
            .expect("a real, parseable predicate");
        let diff = KeyBindingContextPredicate::parse("diff").expect("a real, parseable predicate");
        assert!(contexts_could_overlap(Some(&undo), Some(&diff)));
        assert!(contexts_could_overlap(Some(&diff), Some(&undo)));
    }

    /// Regression for a real bug an audit caught: rebinding an *already-overridden* row a second
    /// time falsely reported a collision against itself. Root cause: the old self-exclusion check
    /// compared `for_binding`'s identity (derived from the real *default* binding) against each
    /// candidate's identity as read off `effective` - but `effective[i]` for an already-overridden
    /// row is the *rebuilt* binding, whose own `BindingIdentity` reports its current (overridden)
    /// keystroke, which never equals the original default's, so the row never excluded itself.
    #[test]
    fn rebinding_an_already_overridden_row_a_second_time_does_not_collide_with_itself() {
        let defaults = crate::default_key_bindings();
        let palette_default = defaults
            .iter()
            .find(|b| b.action().name() == "app::TogglePalette")
            .expect("TogglePalette should be a real default binding");

        // A real override already in place - `TogglePalette` rebound once already.
        let identity = BindingIdentity::of(palette_default);
        let override_entry = KeybindingOverride {
            action: identity.action.clone(),
            context: identity.context.clone(),
            default_keystrokes: identity.default_keystrokes.clone(),
            keystrokes: "ctrl-shift-p".to_string(),
        };
        let effective = effective_key_bindings(std::slice::from_ref(&override_entry));

        // Re-recording the same row and typing the *exact same* chord it already has must not
        // self-collide.
        let collision =
            find_colliding_binding(&defaults, &effective, palette_default, "ctrl-shift-p");
        assert!(
            collision.is_none(),
            "rebinding an already-overridden row onto the same chord it already has must not \
             report a false self-collision"
        );

        // A genuinely different new chord must also not self-collide.
        let collision =
            find_colliding_binding(&defaults, &effective, palette_default, "ctrl-alt-shift-q");
        assert!(collision.is_none());
    }

    #[test]
    fn an_unrelated_keystroke_never_collides() {
        let bindings = crate::default_key_bindings();
        let palette_binding = bindings
            .iter()
            .find(|b| b.action().name() == "app::TogglePalette")
            .expect("TogglePalette should be a real default binding");
        let collision =
            find_colliding_binding(&bindings, &bindings, palette_binding, "ctrl-alt-shift-z");
        assert!(collision.is_none());
    }

    /// A drift guard proving [`BindingIdentity::of`]'s `default_keystrokes` join format really is
    /// what a rebuilt `KeyBinding::load` call (inside [`effective_key_bindings`]) can round-trip
    /// through `Keystroke::parse` - not a format this module invented independently of what gpui
    /// itself expects.
    #[test]
    fn identity_default_keystrokes_round_trips_through_keystroke_parse() {
        for binding in crate::default_key_bindings() {
            for keystroke in binding.keystrokes() {
                let unparsed = keystroke.inner().unparse();
                let reparsed = Keystroke::parse(&unparsed)
                    .unwrap_or_else(|_| panic!("{unparsed} must re-parse"));
                assert_eq!(&reparsed, keystroke.inner());
            }
        }
    }
}
