//! Pure, GPUI-free per-widget text undo/redo (GitHub issue #17): the recorded-operation shape
//! ([`TextEdit`]), the coalescing policy that turns a stream of real keystrokes into natural undo
//! steps ([`EditGroup`]/[`TextHistory::record`]), and a small [`TextField`] wrapper that gives the
//! app's five hand-rolled single-line inputs a real history of their own.
//!
//! Deliberately cross-cutting and top-level (next to `crate::keymap`/`crate::keymap_overrides`)
//! rather than living inside one feature folder: `crate::code_surface::edit_buffer`, `crate::rail`,
//! `crate::palette`, `crate::settings`, `crate::root::new_file` and `crate::sidebar` all drive the
//! exact same mechanism, and duplicating a second, subtly-different coalescing policy per widget
//! is precisely
//! the silent-drift bug class this project's own history (Revision R5.5) already flagged once.
//!
//! ## Two undo systems, never one
//!
//! This is **text** undo, strictly per widget. It is a genuinely separate system from
//! `crate::worktree_history` (Revision R10), which undoes real *git* actions - committing a
//! worktree's changes, discarding a worktree - at the worktree level. The two share the same
//! physical keys (`secondary-z`/`secondary-shift-z`), so they are kept apart structurally, by
//! `crate::default_key_bindings`' own `"text-input"` / `"!terminal && !text-input"` context
//! predicates, not by handler-side guesswork - see that function's own docs for the full scoping
//! rationale. Nothing in this module ever touches `crate::worktree_history`'s state, and vice
//! versa.
//!
//! ## Cursor, not two `Vec`s
//!
//! [`TextHistory`] is one `Vec<EditGroup>` plus a `cursor`: groups `[0..cursor)` are currently
//! *applied*, `[cursor..)` are currently *undone* (available to redo). This is deliberately the
//! same shape `crate::worktree_history::undo::UndoStack` already uses (see that module's own
//! docs), so "a new edit after an undo drops the redo branch" - the standard linear-history rule
//! this issue asks for - falls out of one `truncate(cursor)` in [`TextHistory::record`] rather
//! than ad-hoc bookkeeping.
//!
//! ## The coalescing policy, and why it is exactly this
//!
//! GitHub issue #17 names four group boundaries: **pauses, caret jumps, paste, and programmatic
//! edits**. All four are implemented here, and nothing beyond them is:
//!
//! - **Pause** - [`COALESCE_IDLE`] since the group's own last edit. Time comes in as an explicit
//!   `now: Instant` parameter rather than being read from `Instant::now()` inside, so the policy is
//!   directly testable with real, controlled gaps instead of `sleep`.
//! - **Caret jump** - the new edit's own `before` selection must be exactly the group's current
//!   `after` selection. An arrow key, a click, a `Home`/`End`, or a fresh selection all move the
//!   caret without recording an edit, so the next edit's `before` no longer matches and a new group
//!   starts. This is a stronger and simpler check than comparing raw offsets, and it catches
//!   selection changes (not just caret moves) for free.
//! - **Paste / programmatic** - [`EditKind::Programmatic`] never coalesces in either direction, and
//!   callers additionally [`TextHistory::seal`] around such an edit so the *next* ordinary
//!   keystroke can't merge backwards into it either.
//!
//! An **undo itself** is a fifth, implicit boundary: [`TextHistory::commit_undo`] seals both the
//! group it stepped over and the one it landed on, so whatever the user does next is a new step
//! rather than a continuation of a step they have already walked back past.
//!
//! Deliberately *not* implemented: a word/whitespace boundary rule, or a newline boundary. Real
//! editors differ on both, this issue asks for neither, and each would be one more untested policy
//! knob - this project's standing "don't over-engineer beyond what's actually needed" discipline.
//! `vendor/zed`'s own editor undo grouping is materially larger than this because it also serves
//! multi-buffer excerpts, collaborative transactions and vim mode, none of which exist here.
//! [`MAX_EDITS_PER_GROUP`] is a hard ceiling rather than a policy knob - see its own docs for the
//! real unbounded-growth case it closes.
//!
//! ## Forward-compatible with multi-cursor (issue #14 §3)
//!
//! An [`EditGroup`] holds a `Vec<TextEdit>`, not a single edit, and applies them forward in order /
//! inverts them in reverse order. That is exactly what one multi-cursor edit needs: N simultaneous
//! splices recorded into one group become one undo step. Nothing here assumes a group has exactly
//! one edit, so multi-cursor can land without reshaping this type - only by pushing N edits before
//! sealing instead of one.

use std::ops::Range;
use std::time::{Duration, Instant};

/// How long a pause in typing ends the current undo group. Long enough that an ordinary typing
/// burst (including a moment's thought mid-word) stays one step; short enough that stepping away
/// and coming back doesn't silently extend a step that already feels finished. Sits deliberately
/// above `crate::code_surface::editing::REHIGHLIGHT_DEBOUNCE` (150ms) - re-highlighting wants to
/// fire *within* a typing burst, undo grouping wants to survive one.
pub const COALESCE_IDLE: Duration = Duration::from_millis(600);

/// How many undo groups one history keeps. A bound is real, not decorative: an [`EditGroup`]
/// recorded by [`TextHistory::record_replacement`] for a whole-file external reload holds two full
/// copies of a file that can be up to `code_surface::code_view::MAX_FILE_BYTES` (2 MiB), so an
/// unbounded stack has a real, reachable memory cost. Dropping from the *front* (the oldest,
/// least-likely-to-be-wanted step) is the standard editor behavior; the alternative - refusing to
/// record - would silently make new edits un-undoable, which is worse.
pub const MAX_GROUPS: usize = 200;

/// How many [`TextEdit`]s one [`EditGroup`] may hold before the next edit is forced into a fresh
/// group, whatever the coalescing policy would otherwise say.
///
/// A real bound, not a formality, added during this change's own self-review pass: the
/// idle+contiguity rule bounds a
/// `Type`/`Delete` group to one uninterrupted burst, but a held key with autorepeat never pauses,
/// and [`EditKind::Ime`] has no idle rule at all by design - so a composition the platform never
/// terminates (focus lost, window closed mid-composition) had nothing stopping it growing forever,
/// with every update carrying the whole composing string in *both* `removed` and `inserted`. High
/// enough that no real editing burst or real composition ever reaches it, so this changes nothing
/// a user can observe; low enough to be a real ceiling.
pub const MAX_EDITS_PER_GROUP: usize = 10_000;

/// Total retained bytes across a history's groups, above which the oldest groups are evicted.
///
/// [`MAX_GROUPS`] alone bounds the wrong dimension for the one case that actually costs memory:
/// [`TextHistory::record_replacement`] stores two full document copies per group, and
/// `crate::code_surface::edit_buffer::EditBuffer::reload_from_disk` calls it with content up to
/// `code_view::MAX_FILE_BYTES` (2 MiB). 200 external rewrites of a 2 MiB file - an agent CLI
/// rewriting a file in a loop, which is this app's whole domain - would retain roughly 800 MB in a
/// single buffer, times one per open file. Bounding bytes as well as groups is what actually closes
/// that. Generous enough that no ordinary editing session ever reaches it (a full undo stack of
/// real keystrokes is kilobytes), so this only ever bites the pathological case it exists for.
pub const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;

/// A real caret/selection snapshot, restored verbatim by undo/redo alongside the text. Mirrors
/// `crate::code_surface::edit_buffer::EditBuffer`'s own `selected_range`/`selection_reversed`
/// pair (byte offsets, never UTF-16 - see that type's own docs), so a snapshot round-trips
/// through it without conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionSnapshot {
    pub start: usize,
    pub end: usize,
    /// `true` when the selection was extended leftward from its anchor, i.e. the visible caret
    /// sits at `start` rather than `end`.
    pub reversed: bool,
}

impl SelectionSnapshot {
    /// A collapsed caret at `offset` - the shape every snapshot for the app's caret-less
    /// single-line [`TextField`] inputs takes.
    pub fn caret(offset: usize) -> Self {
        SelectionSnapshot {
            start: offset,
            end: offset,
            reversed: false,
        }
    }

    pub fn of(range: &Range<usize>, reversed: bool) -> Self {
        SelectionSnapshot {
            start: range.start,
            end: range.end,
            reversed,
        }
    }

    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }
}

/// One primitive splice: `removed` occupied `at..at + removed.len()` before it, `inserted`
/// occupies `at..at + inserted.len()` after it. Insert, delete and replace are all this one shape
/// (an insert has an empty `removed`, a delete an empty `inserted`) rather than three enum
/// variants - every consumer would have to handle all three identically anyway, and a single shape
/// makes [`apply_forward`]/[`apply_inverse`] exact mirrors of each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub at: usize,
    pub removed: String,
    pub inserted: String,
}

impl TextEdit {
    /// The byte range this edit occupied *before* it was applied.
    pub fn old_range(&self) -> Range<usize> {
        self.at..self.at + self.removed.len()
    }

    /// The byte range this edit occupies *after* it was applied.
    pub fn new_range(&self) -> Range<usize> {
        self.at..self.at + self.inserted.len()
    }

    /// `true` when this edit changes nothing at all - never recorded, so a *single-edit* undo step
    /// can never be a no-op. A multi-edit group whose individual edits each change something but
    /// whose net effect is identity is a separate case, handled by [`EditGroup::is_net_noop`].
    pub fn is_noop(&self) -> bool {
        self.removed == self.inserted
    }

    /// The real byte cost of retaining this edit - see [`MAX_HISTORY_BYTES`].
    fn byte_cost(&self) -> usize {
        self.removed.len() + self.inserted.len()
    }
}

/// Why an edit happened - the input to the coalescing policy (see this module's own docs). Not a
/// display label; nothing renders this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Ordinary text insertion from a keystroke or an IME commit of a single character.
    Type,
    /// Backspace / Delete / cutting a selection.
    Delete,
    /// A real IME composition step (`replace_and_mark_range`). Every step of one composition
    /// coalesces into one group regardless of how long the composition takes - see
    /// [`TextHistory::can_coalesce`].
    Ime,
    /// Anything the user didn't type character-by-character: paste, an accepted completion, an
    /// external on-disk reload, a programmatic `set`/`clear`. Never coalesces.
    Programmatic,
}

impl EditKind {
    /// The kind an ordinary text replacement resolves to when the caller has no more specific
    /// information: a pure deletion is [`EditKind::Delete`], anything that inserts is
    /// [`EditKind::Type`].
    pub fn for_replacement(inserted: &str) -> Self {
        if inserted.is_empty() {
            EditKind::Delete
        } else {
            EditKind::Type
        }
    }
}

/// One undo step: an ordered run of [`TextEdit`]s plus the real selection on either side of them.
/// See this module's own docs for why this is a `Vec` (multi-cursor forward compatibility) and for
/// what `sealed` means.
#[derive(Debug, Clone, PartialEq)]
pub struct EditGroup {
    pub edits: Vec<TextEdit>,
    /// The selection immediately before this group's first edit - what undo restores.
    pub before: SelectionSnapshot,
    /// The selection immediately after this group's last edit - what redo restores.
    pub after: SelectionSnapshot,
    kind: EditKind,
    last_edit_at: Instant,
    /// A closed group: nothing may ever coalesce into it again, whatever the timing or contiguity
    /// says. Set by [`TextHistory::seal`] at every hard boundary a caller knows about but this
    /// module can't infer (an IME composition ending, a paste, a completion accept, an external
    /// reload).
    sealed: bool,
}

impl EditGroup {
    /// `true` when this whole group's edits, applied in order, leave the text exactly as they
    /// found it - so undoing it would visibly do nothing and the user would have to press Ctrl+Z
    /// again to make progress.
    ///
    /// Real and reachable, not theoretical: a cancelled IME composition produces exactly this
    /// shape. Composing `\u{3042}` records `+"\u{3042}"`, extending it to `\u{3042}\u{3044}`
    /// records `-"\u{3042}" +"\u{3042}\u{3044}"`, and the platform cancelling by sending an empty
    /// preedit records `-"\u{3042}\u{3044}" +""`. Every individual edit changes something, so
    /// [`TextEdit::is_noop`] rejects none of them, they all coalesce into one
    /// [`EditKind::Ime`] group, and that group's net effect is identity.
    ///
    /// Deliberately structural rather than a general splice composition: it recognises a *chain*
    /// (every edit at the same offset, each one's `removed` exactly the previous one's `inserted`)
    /// and reports identity only when the chain's first `removed` equals its last `inserted`.
    /// That is precisely the shape every real IME composition produces, and it is conservative
    /// everywhere else - a group this can't prove is identity is simply kept, never wrongly
    /// dropped. A general "compose N arbitrary splices" implementation would be materially more
    /// code for a case nothing in this app can currently produce.
    fn is_net_noop(&self) -> bool {
        let Some(first) = self.edits.first() else {
            return true;
        };
        let mut expected_removed = first.inserted.as_str();
        for edit in self.edits.iter().skip(1) {
            if edit.at != first.at || edit.removed != expected_removed {
                return false;
            }
            expected_removed = edit.inserted.as_str();
        }
        first.removed == expected_removed
    }

    fn byte_cost(&self) -> usize {
        self.edits.iter().map(TextEdit::byte_cost).sum()
    }
}

/// Splices `edit` into `text` (the forward direction: `removed` becomes `inserted`). Returns
/// `false` without touching `text` if `edit` doesn't actually describe `text`'s current bytes -
/// defensive, so a desynchronized history can never panic `String::replace_range` or silently
/// corrupt real content. Every caller in this crate treats `false` as "refuse the undo/redo",
/// never as "carry on".
pub fn apply_forward(text: &mut String, edit: &TextEdit) -> bool {
    let range = edit.old_range();
    if text.get(range.clone()) != Some(edit.removed.as_str()) {
        return false;
    }
    text.replace_range(range, &edit.inserted);
    true
}

/// The exact mirror of [`apply_forward`] - `inserted` becomes `removed` again.
pub fn apply_inverse(text: &mut String, edit: &TextEdit) -> bool {
    let range = edit.new_range();
    if text.get(range.clone()) != Some(edit.inserted.as_str()) {
        return false;
    }
    text.replace_range(range, &edit.removed);
    true
}

/// The real per-widget undo/redo stack - see this module's own docs for the cursor model and the
/// coalescing policy.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TextHistory {
    groups: Vec<EditGroup>,
    /// Groups `[0..cursor)` are applied; `[cursor..)` are undone.
    cursor: usize,
    /// Running sum of every retained group's [`EditGroup::byte_cost`] - kept incrementally rather
    /// than recomputed, since it is consulted on every recorded edit. See [`MAX_HISTORY_BYTES`].
    bytes: usize,
}

impl TextHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_redo(&self) -> bool {
        self.cursor < self.groups.len()
    }

    /// How many groups are currently recorded - real state, not a debug counter: this crate's own
    /// regression tests assert on it to prove a typing burst really coalesced into one step rather
    /// than N.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Real retained-byte total - see [`MAX_HISTORY_BYTES`]. Test-visible so the byte bound is
    /// asserted against the real running counter rather than inferred from group counts.
    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.bytes
    }

    /// Wipes every recorded step. Only ever correct when the underlying text is being replaced by
    /// a genuinely *different* document (a fresh widget instance, a different file) - never as a
    /// way to deal with an edit this module doesn't understand, which is exactly the "silent
    /// history wipe mid-stack" GitHub issue #17 rules out.
    pub fn reset(&mut self) {
        self.groups.clear();
        self.cursor = 0;
        self.bytes = 0;
    }

    /// Closes the group at the cursor - the newest *applied* one, which is exactly what
    /// [`Self::record`] will see as `groups.last()` after its own `truncate(cursor)`. The next
    /// record therefore always starts a fresh group, whatever the timing/contiguity would
    /// otherwise allow. A no-op when there's nothing to close.
    ///
    /// Keyed off `cursor`, not `groups.last()`. An earlier version guarded on
    /// `cursor == groups.len()` and so did nothing at all whenever a redo branch existed - which
    /// silently voided every caller-driven boundary (paste, cut, an accepted completion, the end
    /// of an IME composition) for the whole window between an undo and the next recorded edit,
    /// exactly the guarantee those call sites exist to provide. Found during this change's own
    /// self-review pass, with a real reproduction: undo, then paste, and the paste merged into the
    /// typing burst before it.
    /// Closing a group is also where a **net no-op** group is dropped rather than sealed - see
    /// [`EditGroup::is_net_noop`] for the real cancelled-IME-composition shape that produces one.
    /// Only ever at the tip (`cursor == groups.len()`): dropping a group with a redo branch above
    /// it would silently renumber that branch, and the case this exists for never has one.
    pub fn seal(&mut self) {
        let Some(index) = self.cursor.checked_sub(1) else {
            return;
        };
        let at_tip = self.cursor == self.groups.len();
        let Some(top) = self.groups.get(index) else {
            return;
        };
        if at_tip && top.is_net_noop() {
            let cost = top.byte_cost();
            self.groups.pop();
            self.bytes = self.bytes.saturating_sub(cost);
            self.cursor = self.groups.len();
            return;
        }
        self.groups[index].sealed = true;
    }

    /// Records one real, already-applied edit, coalescing it into the current group when this
    /// module's own policy allows (see the module docs). Any redo branch is dropped first - the
    /// standard linear-history rule.
    ///
    /// `before`/`after` are the real selection on either side of *this* edit; when the edit
    /// coalesces, the group keeps its original `before` and takes this edit's `after`, so undo
    /// still lands where the whole burst started.
    pub fn record(
        &mut self,
        edit: TextEdit,
        before: SelectionSnapshot,
        after: SelectionSnapshot,
        kind: EditKind,
        now: Instant,
    ) {
        if edit.is_noop() {
            return;
        }
        for dropped in self.groups.drain(self.cursor..) {
            self.bytes = self.bytes.saturating_sub(dropped.byte_cost());
        }
        let cost = edit.byte_cost();
        if let Some(top) = self.groups.last_mut() {
            if Self::can_coalesce(top, before, kind, now) {
                top.edits.push(edit);
                top.after = after;
                top.last_edit_at = now;
                self.bytes += cost;
                self.evict_until_within_budget();
                self.cursor = self.groups.len();
                return;
            }
        }
        self.groups.push(EditGroup {
            edits: vec![edit],
            before,
            after,
            kind,
            last_edit_at: now,
            sealed: false,
        });
        self.bytes += cost;
        self.evict_until_within_budget();
        self.cursor = self.groups.len();
    }

    /// Drops the oldest groups until the history is within **both** [`MAX_GROUPS`] and
    /// [`MAX_HISTORY_BYTES`]. Front-eviction (the oldest, least-likely-to-be-wanted step) is the
    /// standard editor behaviour; refusing to record instead would silently make new edits
    /// un-undoable, which is worse. Always leaves at least one group, so the most recent edit is
    /// undoable however large it is.
    fn evict_until_within_budget(&mut self) {
        while self.groups.len() > 1
            && (self.groups.len() > MAX_GROUPS || self.bytes > MAX_HISTORY_BYTES)
        {
            let dropped = self.groups.remove(0);
            self.bytes = self.bytes.saturating_sub(dropped.byte_cost());
        }
    }

    /// The real policy - see this module's own docs for why it is exactly these rules.
    fn can_coalesce(
        top: &EditGroup,
        before: SelectionSnapshot,
        kind: EditKind,
        now: Instant,
    ) -> bool {
        if top.sealed || top.kind != kind || top.edits.len() >= MAX_EDITS_PER_GROUP {
            return false;
        }
        match kind {
            // One composition is one undo step by definition (GitHub issue #17), however long the
            // user takes over it - so the idle timeout does not apply. A real CJK composition
            // genuinely takes seconds.
            //
            // The caret-continuity rule *does* still apply, and dropping it was a real bug an
            // independent adversarial audit reproduced. A mid-composition Backspace clears
            // `marked_range` as a side effect while deliberately leaving the group open (see
            // `EditBuffer::replace_range_recording`), so with no caret check at all an abandoned
            // composition's group stayed open indefinitely: click somewhere else, compose a
            // completely unrelated word, and both merged into one undo step whose `before` pointed
            // at the first composition's caret. One Ctrl+Z then removed text from two distinct
            // compositions at two distinct offsets. That directly contradicted this module's own
            // stated "a caret jump is a boundary" policy.
            //
            // Keeping the check costs a genuine composition nothing: each `setMarkedText` update
            // leaves the composing caret exactly where the next one starts from, so a real
            // composition chains through it (covered by this module's own multi-step composition
            // test and by `EditBuffer`'s real mid-composition-Backspace one).
            EditKind::Ime => before == top.after,
            EditKind::Programmatic => false,
            EditKind::Type | EditKind::Delete => {
                now.duration_since(top.last_edit_at) < COALESCE_IDLE && before == top.after
            }
        }
    }

    /// Convenience for a caller that has the whole before/after text rather than a splice - an
    /// external on-disk reload, or a programmatic `set`. Records exactly one sealed group, so it
    /// is always its own undo step in both directions.
    pub fn record_replacement(
        &mut self,
        old_text: &str,
        new_text: &str,
        before: SelectionSnapshot,
        after: SelectionSnapshot,
        now: Instant,
    ) {
        self.record(
            TextEdit {
                at: 0,
                removed: old_text.to_string(),
                inserted: new_text.to_string(),
            },
            before,
            after,
            EditKind::Programmatic,
            now,
        );
        self.seal();
    }

    /// Records `edits` as one atomic, already-sealed undo step - this module's own top docs'
    /// "forward-compatible with multi-cursor" design, exercised for real by GitHub issue #26's
    /// Tab/Shift+Tab indenting or dedenting N touched lines at once: N simultaneous splices that
    /// must undo/redo together as a single step, not one at a time (`EditKind::Programmatic`'s own
    /// "never coalesces" rule would otherwise split them into N separate groups if each were
    /// recorded through [`Self::record`] individually, since consecutive splices at *different,
    /// disjoint* line-start offsets never satisfy the caret-continuity check `can_coalesce` needs).
    ///
    /// `edits` must already be in the real order [`apply_forward`] should replay them - i.e. each
    /// edit's own `at` must be valid against the text as it exists *after* every earlier edit in
    /// `edits` has been applied (exactly how [`Self::record`]'s own per-keystroke coalescing already
    /// builds up a multi-edit group one real, already-applied splice at a time - this just accepts
    /// the whole run up front instead). [`Self::commit_undo`]/[`Self::commit_redo`]'s existing
    /// forward/reverse replay handles the rest unchanged. A no-op if `edits` is empty, so a caller
    /// that computed zero real changes (e.g. `Shift+Tab` on lines already at column 0) never pushes
    /// an empty step the user would have to press Ctrl+Z past for nothing.
    pub fn record_group(
        &mut self,
        edits: Vec<TextEdit>,
        before: SelectionSnapshot,
        after: SelectionSnapshot,
        now: Instant,
    ) {
        if edits.is_empty() {
            return;
        }
        for dropped in self.groups.drain(self.cursor..) {
            self.bytes = self.bytes.saturating_sub(dropped.byte_cost());
        }
        let cost: usize = edits.iter().map(TextEdit::byte_cost).sum();
        self.groups.push(EditGroup {
            edits,
            before,
            after,
            kind: EditKind::Programmatic,
            last_edit_at: now,
            sealed: true,
        });
        self.bytes += cost;
        self.evict_until_within_budget();
        self.cursor = self.groups.len();
    }

    /// The group an undo would act on, **without** moving the cursor. The caller applies
    /// [`apply_inverse`] to each of its edits **in reverse order**, restores `before`, and only
    /// then calls [`Self::commit_undo`].
    ///
    /// Peek-then-commit rather than one `undo()` that does both, mirroring
    /// `crate::worktree_history::undo::UndoStack`'s own established discipline (see that module's
    /// docs): every real caller here has to validate that the group genuinely describes the text
    /// it is about to be applied to, and a combined call would leave the cursor moved after a
    /// refusal - a silent desynchronization between the cursor and the content, which is exactly
    /// the failure the validation exists to prevent.
    ///
    /// Returned by value (a clone) rather than by reference: every real caller mutates the same
    /// object that owns this history while applying it, which a live borrow would forbid. Groups
    /// hold only the bytes an edit actually touched, so this is cheap for ordinary typing.
    pub fn peek_undo(&self) -> Option<EditGroup> {
        let index = self.cursor.checked_sub(1)?;
        self.groups.get(index).cloned()
    }

    /// Steps the cursor back over [`Self::peek_undo`]'s group - call only once that group has
    /// genuinely been applied.
    pub fn commit_undo(&mut self) {
        let Some(index) = self.cursor.checked_sub(1) else {
            return;
        };
        self.cursor = index;
        // A group that has been stepped over is closed for good. Redundant for the "undo then
        // type" path (`record`'s own `truncate(cursor)` drops this group entirely before it could
        // be coalesced into), but load-bearing for "undo, redo, then type": after the redo puts
        // the cursor back on the far side of this group, an unsealed group would happily absorb
        // the next keystroke into the step the user just walked back and forth over.
        if let Some(top) = self.groups.get_mut(index) {
            top.sealed = true;
        }
        // ...and so is the group the cursor has just landed *on*, which is the one the next
        // record would otherwise coalesce into. Without this, there is a real, reachable
        // data-losing sequence: type "abc", press Backspace (a second group, by kind),
        // press Ctrl+Z, then type "d" within the idle window - the new character merged into the
        // *original* "abc" group, whose `before` is the empty buffer, so the next Ctrl+Z deleted
        // "abcd" instead of "d". An undo is a real boundary in its own right: whatever the user
        // does next is a new step, never a continuation of a step they have already walked back
        // past.
        self.seal();
    }

    /// The mirror of [`Self::peek_undo`]: the caller applies [`apply_forward`] to each edit **in
    /// order**, restores `after`, and then calls [`Self::commit_redo`].
    pub fn peek_redo(&self) -> Option<EditGroup> {
        self.groups.get(self.cursor).cloned()
    }

    pub fn commit_redo(&mut self) {
        if self.cursor < self.groups.len() {
            self.cursor += 1;
        }
    }
}

/// One of the app's five hand-rolled single-line text inputs (the command-palette query, the rail
/// session filter, the Settings › Keybindings filter, the New file name prompt, and the file
/// tree's inline New File / New Folder / Rename editor - `crate::sidebar::tree_ops::TreeInlineEdit`,
/// which became one of these when GitHub issue #19's tree met issue #17's undo work at a merge)
/// with a real undo history attached. All five are append/backspace-only with no caret of their own - see
/// `crate::rail::AdeApp::handle_filter_key_down`'s own docs for that deliberate scope decision -
/// so every snapshot here is a collapsed caret at the end of the text.
///
/// The `String` is private on purpose: every mutation has to go through a method that records, so
/// a future call site physically cannot bypass the history the way a bare `pub` field would allow.
/// That is the same silent-divergence bug class this project's own audits keep finding.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TextField {
    text: String,
    history: TextHistory,
}

impl TextField {
    pub fn new() -> Self {
        Self::default()
    }

    /// A field that opens *already holding* `text`, with an empty history - the file tree's
    /// inline **rename** editor (GitHub issue #19), which pre-fills with the entry's current
    /// name.
    ///
    /// Deliberately not `new()` followed by [`Self::set`]: that would record the pre-fill as a
    /// real undoable step, so the very first `Ctrl+Z` after opening a rename would blank the
    /// field down to `""` - a state the user never typed and cannot get back to by any other
    /// means. The pre-fill is this widget's *baseline*, not an edit to it, so `can_undo()` is
    /// false until the user genuinely changes something.
    pub fn seeded(text: &str) -> Self {
        Self {
            text: text.to_string(),
            history: TextHistory::new(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Appends `text` at the end (this widget class has no caret to insert at), recording it as
    /// ordinary typing. Returns whether anything changed.
    pub fn push_str(&mut self, text: &str, now: Instant) -> bool {
        if text.is_empty() {
            return false;
        }
        let at = self.text.len();
        let before = SelectionSnapshot::caret(at);
        self.text.push_str(text);
        let after = SelectionSnapshot::caret(self.text.len());
        self.history.record(
            TextEdit {
                at,
                removed: String::new(),
                inserted: text.to_string(),
            },
            before,
            after,
            EditKind::Type,
            now,
        );
        true
    }

    /// Removes the last `char`, matching the `String::pop` these fields used before they had a
    /// history. Returns whether anything was removed.
    pub fn pop(&mut self, now: Instant) -> bool {
        let Some(popped) = self.text.pop() else {
            return false;
        };
        let at = self.text.len();
        let before = SelectionSnapshot::caret(at + popped.len_utf8());
        let after = SelectionSnapshot::caret(at);
        self.history.record(
            TextEdit {
                at,
                removed: popped.to_string(),
                inserted: String::new(),
            },
            before,
            after,
            EditKind::Delete,
            now,
        );
        true
    }

    /// Replaces the whole field programmatically (the `Esc`-clears gesture the rail/Settings
    /// filters have). Recorded as its own sealed step, so `Esc` then Ctrl+Z really brings the
    /// query back rather than silently losing it. Returns whether anything changed.
    pub fn set(&mut self, text: &str, now: Instant) -> bool {
        if self.text == text {
            return false;
        }
        let before = SelectionSnapshot::caret(self.text.len());
        let after = SelectionSnapshot::caret(text.len());
        self.history
            .record_replacement(&self.text, text, before, after, now);
        self.text = text.to_string();
        true
    }

    /// [`Self::set`] to the empty string.
    pub fn clear(&mut self, now: Instant) -> bool {
        self.set("", now)
    }

    /// Drops the text **and** the whole history - for a genuinely new widget instance (the palette
    /// being reopened, a fresh New file prompt), never for an ordinary clear. See
    /// [`TextHistory::reset`]'s own docs.
    pub fn reset(&mut self) {
        self.text.clear();
        self.history.reset();
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// Steps one group back. Returns whether anything was actually undone - `false` both for an
    /// empty history and (defensively) for a group that doesn't match the current text, which
    /// leaves both the text and the cursor untouched rather than corrupting either.
    pub fn undo(&mut self) -> bool {
        let Some(group) = self.history.peek_undo() else {
            return false;
        };
        let mut candidate = self.text.clone();
        for edit in group.edits.iter().rev() {
            if !apply_inverse(&mut candidate, edit) {
                // Refused *before* the cursor moves - see `TextHistory::peek_undo`'s own docs.
                return false;
            }
        }
        self.text = candidate;
        self.history.commit_undo();
        true
    }

    /// The mirror of [`Self::undo`].
    pub fn redo(&mut self) -> bool {
        let Some(group) = self.history.peek_redo() else {
            return false;
        };
        let mut candidate = self.text.clone();
        for edit in &group.edits {
            if !apply_forward(&mut candidate, edit) {
                return false;
            }
        }
        self.text = candidate;
        self.history.commit_redo();
        true
    }

    /// Real recorded-step count - see [`TextHistory::len`]'s own docs.
    #[cfg(test)]
    pub(crate) fn history_len(&self) -> usize {
        self.history.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn insert(at: usize, text: &str) -> TextEdit {
        TextEdit {
            at,
            removed: String::new(),
            inserted: text.to_string(),
        }
    }

    /// Records `text`, one character at a time, as a real typing burst with no pause and no caret
    /// jump - the exact shape a fast typist produces.
    fn type_burst(history: &mut TextHistory, start: usize, text: &str, now: Instant) -> usize {
        let mut at = start;
        for ch in text.chars() {
            let before = SelectionSnapshot::caret(at);
            at += ch.len_utf8();
            let after = SelectionSnapshot::caret(at);
            history.record(
                insert(before.start, &ch.to_string()),
                before,
                after,
                EditKind::Type,
                now,
            );
        }
        at
    }

    #[test]
    fn a_real_typing_burst_coalesces_into_exactly_one_undo_step() {
        let mut history = TextHistory::new();
        let now = t0();
        type_burst(&mut history, 0, "hello", now);
        assert_eq!(
            history.len(),
            1,
            "five consecutive typed characters with no pause and no caret jump must be one real \
             undo step, not five"
        );
        let group = history.peek_undo().expect("one group to undo");
        history.commit_undo();
        assert_eq!(group.edits.len(), 5);
        assert_eq!(group.before, SelectionSnapshot::caret(0));
        assert_eq!(group.after, SelectionSnapshot::caret(5));
    }

    #[test]
    fn a_real_pause_longer_than_the_idle_window_starts_a_new_group() {
        let mut history = TextHistory::new();
        let start = t0();
        let at = type_burst(&mut history, 0, "abc", start);
        assert_eq!(history.len(), 1);
        // A real pause, expressed as a real `Instant` gap rather than a `sleep`.
        let later = start + COALESCE_IDLE + Duration::from_millis(1);
        type_burst(&mut history, at, "def", later);
        assert_eq!(
            history.len(),
            2,
            "a pause longer than COALESCE_IDLE must be a real group boundary"
        );
    }

    #[test]
    fn a_pause_just_inside_the_idle_window_still_coalesces() {
        let mut history = TextHistory::new();
        let start = t0();
        let at = type_burst(&mut history, 0, "abc", start);
        let barely_later = start + COALESCE_IDLE - Duration::from_millis(1);
        type_burst(&mut history, at, "def", barely_later);
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn a_real_caret_jump_between_two_keystrokes_starts_a_new_group() {
        let mut history = TextHistory::new();
        let now = t0();
        type_burst(&mut history, 0, "abc", now);
        // The user clicked (or arrowed) somewhere else: this edit's `before` is no longer the
        // previous group's `after`, with no time having passed at all.
        history.record(
            insert(0, "X"),
            SelectionSnapshot::caret(0),
            SelectionSnapshot::caret(1),
            EditKind::Type,
            now,
        );
        assert_eq!(
            history.len(),
            2,
            "a caret jump is a real group boundary independently of timing"
        );
    }

    #[test]
    fn typing_after_a_selection_change_starts_a_new_group() {
        let mut history = TextHistory::new();
        let now = t0();
        type_burst(&mut history, 0, "abc", now);
        // Shift+Left: the caret offset is unchanged as a *range end*, but the selection is not the
        // same state the group ended in.
        history.record(
            TextEdit {
                at: 2,
                removed: "c".to_string(),
                inserted: "Z".to_string(),
            },
            SelectionSnapshot {
                start: 2,
                end: 3,
                reversed: true,
            },
            SelectionSnapshot::caret(3),
            EditKind::Type,
            now,
        );
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn typing_and_deleting_never_share_a_group() {
        let mut history = TextHistory::new();
        let now = t0();
        let at = type_burst(&mut history, 0, "abc", now);
        history.record(
            TextEdit {
                at: at - 1,
                removed: "c".to_string(),
                inserted: String::new(),
            },
            SelectionSnapshot::caret(at),
            SelectionSnapshot::caret(at - 1),
            EditKind::Delete,
            now,
        );
        assert_eq!(history.len(), 2, "a different EditKind is a real boundary");
    }

    #[test]
    fn consecutive_backspaces_coalesce_into_one_group() {
        let mut history = TextHistory::new();
        let now = t0();
        let mut at = 5usize;
        for _ in 0..3 {
            let before = SelectionSnapshot::caret(at);
            at -= 1;
            history.record(
                TextEdit {
                    at,
                    removed: "x".to_string(),
                    inserted: String::new(),
                },
                before,
                SelectionSnapshot::caret(at),
                EditKind::Delete,
                now,
            );
        }
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn a_programmatic_edit_never_coalesces_in_either_direction() {
        let mut history = TextHistory::new();
        let now = t0();
        type_burst(&mut history, 0, "abc", now);
        history.record(
            insert(3, "pasted"),
            SelectionSnapshot::caret(3),
            SelectionSnapshot::caret(9),
            EditKind::Programmatic,
            now,
        );
        assert_eq!(history.len(), 2);
        history.record(
            insert(9, "more"),
            SelectionSnapshot::caret(9),
            SelectionSnapshot::caret(13),
            EditKind::Programmatic,
            now,
        );
        assert_eq!(
            history.len(),
            3,
            "two consecutive programmatic edits are two real steps"
        );
    }

    #[test]
    fn a_sealed_group_never_accepts_another_edit() {
        let mut history = TextHistory::new();
        let now = t0();
        let at = type_burst(&mut history, 0, "abc", now);
        history.seal();
        type_burst(&mut history, at, "def", now);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn every_step_of_one_ime_composition_is_one_group_however_slow() {
        let mut history = TextHistory::new();
        let start = t0();
        // Three real composition updates, deliberately spread far past COALESCE_IDLE: a real CJK
        // composition can genuinely take seconds, and must still commit as one atomic step.
        history.record(
            insert(0, "\u{304b}"),
            SelectionSnapshot::caret(0),
            SelectionSnapshot::caret(3),
            EditKind::Ime,
            start,
        );
        history.record(
            TextEdit {
                at: 0,
                removed: "\u{304b}".to_string(),
                inserted: "\u{304b}\u{3093}".to_string(),
            },
            SelectionSnapshot::caret(3),
            SelectionSnapshot::caret(6),
            EditKind::Ime,
            start + Duration::from_secs(3),
        );
        history.record(
            TextEdit {
                at: 0,
                removed: "\u{304b}\u{3093}".to_string(),
                inserted: "\u{6f22}".to_string(),
            },
            SelectionSnapshot::caret(6),
            SelectionSnapshot::caret(3),
            EditKind::Ime,
            start + Duration::from_secs(6),
        );
        assert_eq!(
            history.len(),
            1,
            "one IME composition must be exactly one undo step regardless of elapsed time"
        );
        let group = history.peek_undo().expect("the composition group");
        history.commit_undo();
        assert_eq!(group.edits.len(), 3);
        assert_eq!(group.before, SelectionSnapshot::caret(0));
    }

    #[test]
    fn a_new_edit_after_an_undo_drops_the_redo_branch() {
        let mut history = TextHistory::new();
        let now = t0();
        type_burst(&mut history, 0, "abc", now);
        history.seal();
        type_burst(&mut history, 3, "def", now);
        assert_eq!(history.len(), 2);

        history.commit_undo();
        assert!(history.can_redo());

        history.record(
            insert(3, "X"),
            SelectionSnapshot::caret(3),
            SelectionSnapshot::caret(4),
            EditKind::Type,
            now,
        );
        assert!(
            !history.can_redo(),
            "a new edit after an undo must drop the redo branch - standard linear history"
        );
        assert_eq!(history.len(), 2, "the undone group is replaced, not kept");
    }

    #[test]
    fn typing_immediately_after_an_undo_does_not_reopen_the_undone_group() {
        let mut history = TextHistory::new();
        let now = t0();
        type_burst(&mut history, 0, "abc", now);
        history.commit_undo();
        // Same instant, and `before` deliberately equal to the *remaining* state - without the
        // seal inside `undo`, this would merge into the group that was just stepped over.
        type_burst(&mut history, 0, "z", now);
        assert_eq!(history.len(), 1);
        assert!(history.can_undo());
        let group = history.peek_undo().expect("the new group");
        history.commit_undo();
        assert_eq!(group.edits.len(), 1);
    }

    #[test]
    fn undo_and_redo_walk_the_cursor_without_dropping_groups() {
        let mut history = TextHistory::new();
        let now = t0();
        type_burst(&mut history, 0, "abc", now);
        history.seal();
        type_burst(&mut history, 3, "def", now);

        assert!(history.can_undo() && !history.can_redo());
        history.commit_undo();
        history.commit_undo();
        assert!(!history.can_undo() && history.can_redo());
        assert_eq!(history.len(), 2, "undo never discards groups");
        history.commit_redo();
        history.commit_redo();
        assert!(history.can_undo() && !history.can_redo());
    }

    #[test]
    fn a_noop_edit_is_never_recorded() {
        let mut history = TextHistory::new();
        history.record(
            TextEdit {
                at: 0,
                removed: "a".to_string(),
                inserted: "a".to_string(),
            },
            SelectionSnapshot::caret(0),
            SelectionSnapshot::caret(1),
            EditKind::Type,
            t0(),
        );
        assert!(history.is_empty());
    }

    #[test]
    fn the_group_cap_drops_the_oldest_step_and_keeps_the_cursor_at_the_tip() {
        let mut history = TextHistory::new();
        let now = t0();
        for index in 0..(MAX_GROUPS + 10) {
            history.record(
                insert(index, "x"),
                SelectionSnapshot::caret(index),
                SelectionSnapshot::caret(index + 1),
                EditKind::Programmatic,
                now,
            );
        }
        assert_eq!(history.len(), MAX_GROUPS);
        assert!(history.can_undo());
        assert!(!history.can_redo(), "the cursor must still be at the tip");
    }

    #[test]
    fn apply_forward_and_inverse_are_exact_mirrors_over_real_text() {
        let mut text = "hello world".to_string();
        let edit = TextEdit {
            at: 6,
            removed: "world".to_string(),
            inserted: "there".to_string(),
        };
        assert!(apply_forward(&mut text, &edit));
        assert_eq!(text, "hello there");
        assert!(apply_inverse(&mut text, &edit));
        assert_eq!(text, "hello world");
    }

    #[test]
    fn applying_an_edit_that_does_not_describe_the_text_is_refused_not_applied() {
        let mut text = "hello".to_string();
        let edit = TextEdit {
            at: 0,
            removed: "goodbye".to_string(),
            inserted: "x".to_string(),
        };
        assert!(!apply_forward(&mut text, &edit));
        assert_eq!(
            text, "hello",
            "a refused edit must leave the text untouched"
        );
    }

    /// [`TextField::seeded`]'s whole contract, asserted directly in the module that owns it
    /// rather than only through the file tree's rename editor that uses it: the pre-fill is the
    /// field's *baseline*, not an undoable edit.
    ///
    /// The contrast with `new()` + `set(..)` is asserted in the same test, because that is the
    /// construction `seeded` exists to prevent and the difference is invisible from the text
    /// alone - both hold `"README.md"` immediately after construction, and only `can_undo()`
    /// distinguishes them until the first Ctrl+Z blanks one of them to `""`.
    #[test]
    fn text_field_seeded_holds_its_text_with_no_undoable_step_behind_it() {
        let seeded = TextField::seeded("README.md");
        assert_eq!(seeded.as_str(), "README.md");
        assert!(
            !seeded.can_undo(),
            "the pre-fill must not be recorded, or the first Ctrl+Z would blank a field the user \
             never typed into"
        );
        assert_eq!(seeded.history_len(), 0);

        let mut via_set = TextField::new();
        via_set.set("README.md", t0());
        assert_eq!(
            via_set.as_str(),
            seeded.as_str(),
            "same visible text - which is exactly why this bug would not show up in a text-only \
             assertion"
        );
        assert!(
            via_set.can_undo(),
            "sanity check: the construction `seeded` replaces really does record the pre-fill, \
             so this test is genuinely discriminating"
        );

        // A real edit on top of a seeded field undoes back to the baseline, not past it.
        let mut field = TextField::seeded("README.md");
        field.push_str("x", t0());
        assert_eq!(field.as_str(), "README.mdx");
        assert!(field.undo());
        assert_eq!(field.as_str(), "README.md");
        assert!(!field.undo(), "there is nothing behind the baseline");
    }

    #[test]
    fn text_field_typing_then_undo_restores_the_text_before_the_burst() {
        let mut field = TextField::new();
        let now = t0();
        for ch in "hello".chars() {
            field.push_str(&ch.to_string(), now);
        }
        assert_eq!(field.as_str(), "hello");
        assert_eq!(field.history_len(), 1);
        assert!(field.undo());
        assert_eq!(field.as_str(), "");
        assert!(!field.undo());
        assert!(field.redo());
        assert_eq!(field.as_str(), "hello");
    }

    #[test]
    fn text_field_escape_clear_is_undoable() {
        let mut field = TextField::new();
        let now = t0();
        field.push_str("main", now);
        assert!(field.clear(now));
        assert_eq!(field.as_str(), "");
        assert!(field.undo());
        assert_eq!(
            field.as_str(),
            "main",
            "clearing a filter with Esc must be a real, undoable step, not a silent loss"
        );
    }

    #[test]
    fn text_field_backspaces_coalesce_and_undo_as_one_step() {
        let mut field = TextField::new();
        let now = t0();
        field.push_str("abcdef", now);
        for _ in 0..3 {
            field.pop(now);
        }
        assert_eq!(field.as_str(), "abc");
        assert!(field.undo());
        assert_eq!(field.as_str(), "abcdef");
    }

    #[test]
    fn text_field_reset_drops_the_history_too() {
        let mut field = TextField::new();
        let now = t0();
        field.push_str("abc", now);
        field.reset();
        assert_eq!(field.as_str(), "");
        assert!(
            !field.can_undo(),
            "a reset field is a genuinely new widget instance - its predecessor's history must \
             not be reachable from it"
        );
    }

    #[test]
    fn text_field_handles_a_real_multi_byte_character_without_splitting_it() {
        let mut field = TextField::new();
        let now = t0();
        field.push_str("caf\u{e9}", now);
        field.push_str("\u{1f600}", now);
        assert_eq!(field.as_str(), "caf\u{e9}\u{1f600}");
        assert!(field.pop(now));
        assert_eq!(field.as_str(), "caf\u{e9}");
        assert!(field.undo());
        assert_eq!(field.as_str(), "caf\u{e9}\u{1f600}");
    }

    /// Regression for a real, reachable data-losing sequence found in self-review: `commit_undo`
    /// sealed only the group it stepped *over*, leaving the one it landed on open, so the very next
    /// keystroke could merge into a step the user had already walked back past.
    #[test]
    fn typing_right_after_an_undo_never_merges_into_the_group_the_cursor_landed_on() {
        let mut history = TextHistory::new();
        let now = t0();
        // "abc", then a Backspace - a second group, split by kind, not by time.
        let at = type_burst(&mut history, 0, "abc", now);
        history.record(
            TextEdit {
                at: at - 1,
                removed: "c".to_string(),
                inserted: String::new(),
            },
            SelectionSnapshot::caret(at),
            SelectionSnapshot::caret(at - 1),
            EditKind::Delete,
            now,
        );
        assert_eq!(history.len(), 2);

        // Undo the Backspace. The caret lands exactly where the "abc" burst ended, and no time has
        // passed - every other coalescing condition is satisfied.
        history.commit_undo();
        type_burst(&mut history, 3, "d", now);

        assert_eq!(
            history.len(),
            2,
            "the new character must be its own group - one on top of the surviving \"abc\" group, \
             not merged into it"
        );
        let group = history.peek_undo().expect("the new group");
        assert_eq!(
            group.edits.len(),
            1,
            "undoing must remove only the character just typed, never the whole burst the user \
             had already stepped back over"
        );
    }

    /// The second manifestation of the same bug: `seal` guarded on `cursor == groups.len()`, so
    /// every caller-driven boundary silently did nothing while a redo branch existed - and a paste
    /// (recorded as ordinary `Type`, bounded *only* by those seals) merged backwards into the
    /// typing before it.
    #[test]
    fn a_seal_still_takes_effect_while_a_redo_branch_exists() {
        let mut history = TextHistory::new();
        let now = t0();
        let at = type_burst(&mut history, 0, "abc", now);
        history.record(
            insert(at, "X"),
            SelectionSnapshot::caret(at),
            SelectionSnapshot::caret(at + 1),
            EditKind::Programmatic,
            now,
        );
        assert_eq!(history.len(), 2);

        // Step back over the programmatic edit, so a redo branch now exists.
        history.commit_undo();
        assert!(history.can_redo());

        // Exactly what `AdeApp::handle_editor_paste_action` does around a real paste.
        history.seal();
        history.record(
            insert(3, "PASTED"),
            SelectionSnapshot::caret(3),
            SelectionSnapshot::caret(9),
            EditKind::Type,
            now,
        );
        history.seal();

        assert_eq!(
            history.len(),
            2,
            "the paste must be its own group on top of the surviving \"abc\" one"
        );
        let group = history.peek_undo().expect("the paste's own group");
        assert_eq!(
            group.edits.len(),
            1,
            "a seal must close the group at the cursor even while a redo branch exists - \
             otherwise the paste merges into the typing before it and one Ctrl+Z removes both"
        );
    }

    #[test]
    fn no_group_ever_grows_past_the_per_group_edit_ceiling() {
        let mut history = TextHistory::new();
        let now = t0();
        // An IME composition is the real unbounded case: its coalescing rule has no idle check at
        // all by design, so only the ceiling can stop it.
        for index in 0..(MAX_EDITS_PER_GROUP + 5) {
            history.record(
                insert(index, "x"),
                SelectionSnapshot::caret(index),
                SelectionSnapshot::caret(index + 1),
                EditKind::Ime,
                now,
            );
        }
        assert_eq!(
            history.len(),
            2,
            "the ceiling must have forced a second group"
        );
        let group = history.peek_undo().expect("the overflow group");
        assert_eq!(group.edits.len(), 5);
    }

    /// Audit finding: a cancelled IME composition left a group whose individual edits each changed
    /// something but whose net effect was identity - `can_undo()` reported `true` and Ctrl+Z
    /// visibly did nothing.
    #[test]
    fn a_cancelled_composition_leaves_no_dead_undo_step_behind() {
        let mut history = TextHistory::new();
        let now = t0();
        history.record(
            insert(5, "\u{3042}"),
            SelectionSnapshot::caret(5),
            SelectionSnapshot::caret(8),
            EditKind::Ime,
            now,
        );
        history.record(
            TextEdit {
                at: 5,
                removed: "\u{3042}".to_string(),
                inserted: "\u{3042}\u{3044}".to_string(),
            },
            SelectionSnapshot::caret(8),
            SelectionSnapshot::caret(11),
            EditKind::Ime,
            now,
        );
        // The platform cancels by sending an empty preedit.
        history.record(
            TextEdit {
                at: 5,
                removed: "\u{3042}\u{3044}".to_string(),
                inserted: String::new(),
            },
            SelectionSnapshot::caret(11),
            SelectionSnapshot::caret(5),
            EditKind::Ime,
            now,
        );
        assert_eq!(
            history.len(),
            1,
            "sanity: all three coalesced into one group"
        );
        history.seal();

        assert!(
            history.is_empty(),
            "a group whose net effect is identity must be dropped, not sealed - otherwise \
             can_undo() is true and Ctrl+Z visibly does nothing"
        );
        assert!(!history.can_undo());
    }

    /// The conservative half: a group that really does change something must never be mistaken
    /// for a net no-op and dropped.
    #[test]
    fn a_composition_that_really_committed_is_never_dropped_as_a_no_op() {
        let mut history = TextHistory::new();
        let now = t0();
        history.record(
            insert(5, "\u{3042}"),
            SelectionSnapshot::caret(5),
            SelectionSnapshot::caret(8),
            EditKind::Ime,
            now,
        );
        history.record(
            TextEdit {
                at: 5,
                removed: "\u{3042}".to_string(),
                inserted: "\u{6f22}".to_string(),
            },
            SelectionSnapshot::caret(8),
            SelectionSnapshot::caret(8),
            EditKind::Ime,
            now,
        );
        history.seal();
        assert_eq!(history.len(), 1);
        assert!(history.can_undo());
    }

    /// The IME arm keeps its time exemption but not its caret exemption - a real composition
    /// chains through the caret check, an abandoned one does not.
    #[test]
    fn a_composition_coalesces_across_a_long_pause_but_not_across_a_caret_jump() {
        let mut history = TextHistory::new();
        let start = t0();
        history.record(
            insert(5, "\u{3042}"),
            SelectionSnapshot::caret(5),
            SelectionSnapshot::caret(8),
            EditKind::Ime,
            start,
        );
        // Seconds later, still the same composition, caret exactly where the last update left it.
        history.record(
            TextEdit {
                at: 5,
                removed: "\u{3042}".to_string(),
                inserted: "\u{3042}\u{3044}".to_string(),
            },
            SelectionSnapshot::caret(8),
            SelectionSnapshot::caret(11),
            EditKind::Ime,
            start + Duration::from_secs(8),
        );
        assert_eq!(
            history.len(),
            1,
            "a real composition must survive an arbitrarily long pause"
        );

        // A real caret jump, then a completely unrelated composition - no time passes at all.
        history.record(
            insert(0, "\u{304b}"),
            SelectionSnapshot::caret(0),
            SelectionSnapshot::caret(3),
            EditKind::Ime,
            start + Duration::from_secs(8),
        );
        assert_eq!(
            history.len(),
            2,
            "but a caret jump is a real boundary for a composition too - one Ctrl+Z must never \
             remove text from two compositions at two different offsets"
        );
    }

    /// Audit finding: `MAX_GROUPS` bounds the group *count*, not bytes, while
    /// `record_replacement` stores two whole-document copies per group - so repeated external
    /// rewrites of a large file could retain hundreds of megabytes.
    #[test]
    fn a_run_of_whole_document_replacements_is_bounded_by_real_bytes_not_just_group_count() {
        let mut history = TextHistory::new();
        let now = t0();
        let big = "x".repeat(512 * 1024);
        let mut previous = String::new();
        for index in 0..80 {
            let next = format!("{big}{index}");
            history.record_replacement(
                &previous,
                &next,
                SelectionSnapshot::caret(0),
                SelectionSnapshot::caret(0),
                now,
            );
            previous = next;
        }
        assert!(
            history.len() < 80,
            "the byte budget must have evicted older steps well before the group cap: {} groups",
            history.len()
        );
        assert!(
            history.retained_bytes() <= MAX_HISTORY_BYTES,
            "retained {} bytes, budget is {MAX_HISTORY_BYTES}",
            history.retained_bytes()
        );
        assert!(
            history.can_undo(),
            "the most recent step must always stay undoable, however large"
        );
    }

    #[test]
    fn dropping_a_redo_branch_releases_its_retained_bytes() {
        let mut history = TextHistory::new();
        let now = t0();
        history.record_replacement(
            "",
            &"y".repeat(4096),
            SelectionSnapshot::caret(0),
            SelectionSnapshot::caret(0),
            now,
        );
        let with_branch = history.retained_bytes();
        assert!(with_branch >= 4096);
        history.commit_undo();
        // A fresh edit discards the redo branch - its bytes must go with it.
        history.record(
            insert(0, "z"),
            SelectionSnapshot::caret(0),
            SelectionSnapshot::caret(1),
            EditKind::Type,
            now,
        );
        assert!(
            history.retained_bytes() < with_branch,
            "the discarded redo branch's bytes must be released, not leaked into the running total"
        );
    }

    // GitHub issue #26: `record_group` - the real multi-edit-group primitive Tab/Shift+Tab
    // indenting/dedenting N lines at once needs (see `EditBuffer::indent_lines`'s own docs).

    #[test]
    fn record_group_pushes_every_edit_into_one_real_undo_step() {
        let mut history = TextHistory::new();
        let now = t0();
        history.record_group(
            vec![insert(8, "  "), insert(0, "  ")],
            SelectionSnapshot::of(&(0..10), false),
            SelectionSnapshot::of(&(0..14), false),
            now,
        );
        assert_eq!(
            history.len(),
            1,
            "N simultaneous edits must be one group, not N"
        );
        let group = history.peek_undo().expect("one group to undo");
        assert_eq!(group.edits.len(), 2);
        assert_eq!(group.before, SelectionSnapshot::of(&(0..10), false));
        assert_eq!(group.after, SelectionSnapshot::of(&(0..14), false));
    }

    #[test]
    fn record_group_is_a_real_no_op_for_an_empty_edit_list() {
        let mut history = TextHistory::new();
        history.record_group(
            Vec::new(),
            SelectionSnapshot::caret(0),
            SelectionSnapshot::caret(0),
            t0(),
        );
        assert!(
            history.is_empty(),
            "an empty edit list must never push a real, empty undo step"
        );
    }

    #[test]
    fn record_group_never_coalesces_with_a_later_group_of_the_same_kind() {
        // `EditKind::Programmatic`'s own "never coalesces" rule (shared with
        // `record_replacement`) must still hold for a grouped multi-edit record - two separate
        // `Tab` presses stay two separate undo steps, never merging into one.
        let mut history = TextHistory::new();
        let now = t0();
        history.record_group(
            vec![insert(0, "  ")],
            SelectionSnapshot::caret(0),
            SelectionSnapshot::caret(2),
            now,
        );
        history.record_group(
            vec![insert(2, "  ")],
            SelectionSnapshot::caret(2),
            SelectionSnapshot::caret(4),
            now,
        );
        assert_eq!(
            history.len(),
            2,
            "two real record_group calls must stay two real groups"
        );
    }

    #[test]
    fn record_group_drops_a_real_redo_branch_like_an_ordinary_record_does() {
        let mut history = TextHistory::new();
        let now = t0();
        type_burst(&mut history, 0, "ab", now);
        history.commit_undo();
        assert!(history.can_redo());
        history.record_group(
            vec![insert(0, "X")],
            SelectionSnapshot::caret(0),
            SelectionSnapshot::caret(1),
            now,
        );
        assert!(
            !history.can_redo(),
            "linear history: a grouped edit after an undo must discard the redo branch too"
        );
    }

    #[test]
    fn record_group_apply_forward_and_inverse_replay_a_real_multi_line_indent() {
        // The real shape `EditBuffer::indent_lines` produces for a 3-line indent: edits collected
        // bottom-to-top, each `at` valid against the text as it existed after the earlier (lower)
        // edits in the list were already applied - see that method's own docs.
        let mut text = "one\ntwo\nthree\n".to_string();
        let edits = vec![insert(8, "  "), insert(4, "  "), insert(0, "  ")];
        for edit in &edits {
            assert!(apply_forward(&mut text, edit));
        }
        assert_eq!(text, "  one\n  two\n  three\n");
        for edit in edits.iter().rev() {
            assert!(apply_inverse(&mut text, edit));
        }
        assert_eq!(text, "one\ntwo\nthree\n");
    }
}
