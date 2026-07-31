//! Pure logic for Surface C's File view real text editing (Revision R8.5a): an [`EditBuffer`]
//! holds one open file's live, in-memory edited text plus a real cursor/selection/IME-composition
//! state, and every mutation (typing, IME composition, Backspace/Delete/Enter, arrow-key/click
//! cursor movement) goes through it. Deliberately `gpui`-window-free (only `gpui::SharedString`,
//! transitively via [`code_view::RenderedLine`], is used, for plain highlighted-text data),
//! mirroring `crate::code_surface::code_view`'s own split between pure logic and `crate::root`'s live `Div`/
//! `Window` construction - see that module's own top doc comment for the same convention. The
//! real GPUI wiring (`EntityInputHandler`, keyboard actions, painting a real cursor/selection)
//! lives in `crate::code_surface::editing`, which drives this module's methods.
//!
//! ## Real multi-step undo/redo (GitHub issue #17)
//!
//! Ordinary editing - typing, IME composition, Backspace/Delete/Enter, paste, cut, an accepted
//! completion - funnels through exactly two public methods ([`Self::replace_range`] and
//! [`Self::replace_and_mark_range`]) and one private splice ([`Self::splice_lines`]). That choke
//! point is what makes a real history cheap to attach correctly: both public methods record the
//! splice they just performed - the byte offset, the exact text removed, the exact text inserted,
//! and the real selection on either side - into [`Self::history`]
//! (`crate::text_history::TextHistory`), which owns the coalescing policy. See that module's own
//! docs for the policy itself and for why it lives there rather than here.
//!
//! Two real exceptions to that funnel, both deliberate and both recording:
//! [`Self::reload_from_disk`] replaces the whole buffer (and rebuilds the derived tables directly
//! rather than splicing), and [`Self::content`] is a `pub` field, so the funnel is a convention
//! this type's own methods keep, not something the type system enforces the way
//! `crate::text_history::TextField` does for the app's single-line inputs. [`Self::undo`]/
//! [`Self::redo`] therefore validate a group against the buffer's real current bytes before
//! applying any of it, and refuse outright rather than half-applying - see [`Self::undo`]'s own
//! docs.
//!
//! [`Self::undo`]/[`Self::redo`] replay a whole group through the same `splice_lines` every real
//! edit already uses, so the incremental line/UTF-16 tables below stay correct by construction
//! rather than by a second, parallel implementation. They restore the recorded caret **and**
//! selection, never just the text.
//!
//! Recording is suppressed while a group is being replayed ([`Self::replaying`]) - otherwise an
//! undo would immediately record itself as a fresh edit and the stack could never move backwards.
//!
//! What is deliberately *not* here: history does not survive the buffer itself. `AdeApp::
//! edit_buffers` keeps one buffer per open file for as long as the tab lives, so switching tabs and
//! back preserves that file's history (a real regression test covers exactly that), but closing the
//! tab or switching worktree drops the buffer and its history with it - explicitly out of scope per
//! GitHub issue #17's own checklist.
//!
//! ### Multi-cursor edits are one real undo step (issue #28, landing on top of issue #17)
//!
//! `crate::text_history`'s own docs call this out as a design goal it was built to accommodate: an
//! [`text_history::EditGroup`] holds a `Vec<TextEdit>`, not a single edit, specifically so that N
//! simultaneous multi-cursor splices recorded into one group become one undo step. [`Self::
//! apply_at_every_cursor`] does exactly that - it calls [`TextHistory::record`] once per cursor's
//! own splice, chaining each call's `before` snapshot to the previous call's `after` so the
//! coalescing policy's own caret-continuity rule (`before == top.after`) merges every one of them
//! into a single group rather than starting a fresh one per cursor. The group's *stored*
//! `before`/`after` (what undo/redo actually restore) are the chain's real first/last values, not
//! synthetic ones - see that method's own docs for exactly which cursor's snapshot each is.
//!
//! **A real, documented limitation, not a fabricated "it just works"**: [`text_history::
//! SelectionSnapshot`] - shared with every other real text widget in this app
//! (`crate::text_history::TextField`) - holds exactly one caret/selection, mirroring this buffer's
//! own primary [`Self::selected_range`]/[`Self::selection_reversed`] pair. It has no way to
//! represent [`Self::secondary_cursors`]. So undoing/redoing a multi-cursor edit correctly restores
//! **all of the text** as one atomic step (every cursor's own edit reverses/replays together,
//! genuinely one Ctrl+Z), but only restores the **primary** cursor's own caret/selection
//! precisely; every secondary cursor from that edit is not reconstructed by undo/redo, and the
//! buffer is left in ordinary single-cursor state afterward. Extending `SelectionSnapshot` to a real
//! multi-cursor shape would touch every one of `crate::text_history`'s own consumers (`rail`,
//! `palette`, `settings`, `root::new_file`), not just this buffer - real, separate, cross-cutting
//! work outside this one sub-issue's scope, left as a documented gap rather than guessed at.
//!
//! ## Multi-cursor: `Self::secondary_cursors`
//!
//! [`Self::selected_range`]/[`Self::selection_reversed`] remain the *primary* cursor exactly as
//! before this revision (Revision R13, issue #28) - every existing single-cursor method
//! (`Self::move_left`, `Self::replace_range`, etc.) keeps its exact prior behavior, unchanged,
//! whenever [`Self::secondary_cursors`] is empty (the default, and the only state a freshly
//! constructed buffer or a plain click ever leaves it in). [`Self::secondary_cursors`] holds every
//! *additional* real cursor/selection - VS Code-style `Ctrl+D` ("Add Selection To Next Find
//! Match"), `Ctrl+Shift+L` ("Select All Occurrences"), and Alt+click all populate it (see
//! [`Self::select_word_or_add_next_occurrence`], [`Self::select_all_occurrences`],
//! [`Self::add_cursor_at`]).
//!
//! The design choice made here: rather than replacing `selected_range`/`selection_reversed` with a
//! `Vec<Selection>` everywhere (which would have meant rewriting every one of this file's own
//! already-correct, already-tested single-cursor methods, and every call site across
//! `crate::code_surface::editing`'s ~2900 lines, to index into a vector instead of reading a plain
//! field), the primary cursor keeps its own plain fields and every multi-cursor operation is
//! layered on top as a genuinely new, additive code path:
//!
//! - **Simultaneous edits** ([`Self::replace_range`]/[`Self::backspace`]/[`Self::delete_forward`],
//!   used by every keystroke, paste, and IME commit - `crate::code_surface::editing`'s
//!   `EntityInputHandler` impl and `Editor*` action handlers already funnel through these three,
//!   unchanged): each one now checks `Self::secondary_cursors` first and, if non-empty, routes
//!   through [`Self::apply_at_every_cursor`] instead of its own prior single-range body - one
//!   atomic multi-cursor edit, not N independent ones (see that method's own docs for the real
//!   right-to-left splice ordering this relies on for correctness).
//! - **Cursor movement** (`Self::move_left`/`Self::move_right`/.../`Self::select_down`, driving
//!   every arrow-key `Editor*` action): each now routes through [`Self::move_every_cursor`] when
//!   multiple cursors are active, so an arrow key moves *every* real cursor together instead of
//!   silently stranding every secondary wherever `Ctrl+D` left it - see that method's own docs for
//!   the one honest, narrow scope cut (no independent per-cursor sticky goal-column) this makes.
//! - **Collision merging** ([`Self::merge_colliding_cursors`]): called at the end of every
//!   operation above that could produce two overlapping/identical cursors (an edit, a movement, or
//!   adding a new cursor) - VS Code's own "cursors merge when they collide" behavior, not a manual
//!   step the caller has to remember.
//! - **A plain, unmodified click always collapses back to one cursor**: [`Self::move_to`]/
//!   [`Self::select_to`] (the real target of every ordinary/shift-click, and the same primitives
//!   every single-cursor keyboard method already funneled through) now clear
//!   [`Self::secondary_cursors`] as their first step - matching every real multi-cursor editor's
//!   own "clicking without a modifier always means one cursor" rule. `Esc`
//!   ([`Self::collapse_to_single_cursor`]) does the same thing as an explicit action.
//!
//! **A documented, honest gap**: `EditBuffer::replace_and_mark_range` (real IME composition) and
//! any *explicit*-range call to `Self::replace_range` (e.g. a completion's own text-edit
//! application) do **not** route through the multi-cursor path even while
//! `Self::secondary_cursors` is non-empty - composing text or accepting a completion while
//! multiple cursors are active only ever affects the primary, leaving every secondary cursor's own
//! position stale relative to the resulting edit. A real, narrow edge case (composing CJK/accented
//! input, or accepting an LSP completion, while mid multi-cursor selection is rare in practice),
//! left undone rather than guessed at, per this phase's own priority: a correct core model plus
//! `Ctrl+D`/simultaneous typing-deleting-pasting, not every possible interaction of every existing
//! subsystem with a genuinely new one.
//!
//! **Also left undone, both flagged in issue #28 itself as more peripheral than the above**:
//! Alt+Shift+drag column selection (this editor has no mouse-drag-to-select of *any* kind yet -
//! only click and shift-click, so a column-selection variant of a feature that doesn't exist yet
//! is out of reach without first building ordinary drag-select, itself a separate, real piece of
//! work), and `Ctrl+K Ctrl+D`'s own "skip" is implemented ([`Self::skip_current_occurrence`]) but
//! deliberately does not attempt to also support skipping a cursor other than the most-recently-
//! added one (matching VS Code's own real behavior, which also only ever acts on the most recent
//! selection).
//!
//! ## Diff/Merge views stay 100% read-only
//!
//! Only the File view gets real editing this phase. `crate::code_surface::
//! render_diff_file_detail` and `crate::merge::render`'s conflict-resolution columns
//! are untouched - neither renders through [`EditBuffer`] or gains any `EntityInputHandler`
//! wiring, so a hand-edit inside a diff hunk or an in-progress merge conflict resolution is still
//! not possible, exactly as before this phase.
//!
//! ## Re-highlighting cost, and why typing doesn't re-run `tree-sitter` on every keystroke
//!
//! A real, measured number, not a guess: highlighting this crate's own largest real `.rs` file
//! (`crates/app/src/lsp/client.rs`, 3618 lines / ~180KB) end to end
//! (`code_view::highlight_rust` + `code_view::build_lines`) took **~75ms** in a debug build
//! (averaged over 10 runs). That's nowhere near the ~5-8ms/frame budget needed for typing to not
//! visibly lag at 60fps - re-running this on every keystroke for a file that size would make the
//! whole app stutter on every character typed, the exact "expensive recomputation on every
//! render/keystroke" bug class this project has hit and fixed repeatedly (see BUILD-LOG.md's
//! Revision R9a, which independently measured a *different* code path - per-hunk Diff/Merge
//! highlighting - at up to ~80ms on the same file and added a 300-line cap for it).
//!
//! So [`EditBuffer::replace_range`]/[`EditBuffer::replace_and_mark_range`] (every real edit -
//! typing, paste, Backspace/Delete/Enter all reduce to one of these two) never call a highlighter
//! directly. Instead they call [`EditBuffer::splice_lines`], which re-derives just the line(s) the
//! edit actually touches - via [`code_view::build_lines`] with an **empty** span list, restricted
//! to that narrow region, never the whole buffer (see that method's own docs for the real,
//! measured whole-buffer-per-keystroke performance bug this fixes) - a cheap, real, honest
//! immediate result (every visible row's *text* is always exactly correct the instant you type).
//! Only the touched line(s)' own runs reset to plain [`code_view::HighlightKind::Text`] until real
//! highlighting catches up; an untouched line keeps whatever real highlighting it already had (a
//! real improvement over an earlier, whole-buffer-rebuild version of this same idea, which
//! flickered the *entire* file back to plain on every single keystroke, however far from the edit
//! point). Either way, `EditBuffer::highlight_dirty` is set `true` whenever this happens, so
//! `crate::root::AdeApp` knows a real re-highlight (which fully replaces every line's own runs,
//! not just the touched ones) is owed.
//!
//! `crate::root::AdeApp` debounces the real `tree-sitter` re-highlight a short interval after the
//! last keystroke (see `AdeApp::schedule_rehighlight`'s own docs) rather than either running it
//! inline (too slow, per the measurement above) or trying to incrementally patch the previous
//! frame's stale colored spans onto the new text (rejected as needless complexity/risk: a
//! naively-shifted stale span landing on the *wrong* new token would be a more misleading result
//! than the plain, honestly-uncolored text this module shows in the meantime). Once the debounce
//! fires, [`EditBuffer::apply_highlight`] installs the real result - guarded by a `content`
//! equality check so a highlight computed against now-stale content (a further keystroke arrived
//! while it was computing) is discarded rather than clobbering newer, correct plain text with
//! older, wrong-position colors.

use std::ops::Range;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use unicode_segmentation::UnicodeSegmentation;

use crate::code_surface::code_view;
use crate::code_surface::indent;
use crate::language::HighlighterFn;
use crate::text_history::{self, EditKind, SelectionSnapshot, TextEdit, TextHistory};

/// A character's real class for word-wise caret movement/selection (GitHub issue #27) - see
/// [`EditBuffer::previous_word_boundary`]'s own docs for why this app hand-classifies rather
/// than using `unicode_segmentation`'s UAX #29 word boundaries for this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Whitespace,
    /// A letter, digit, or underscore - grouped together so `foo_bar123` is one real word, not
    /// three.
    Word,
    /// Anything else (`.`, `(`, `)`, `-`, ...) - a real code editor's own word-navigation stops
    /// at these individually from surrounding word text, but groups a *run* of them together
    /// (`()` is one hop, not two), matching this app's own real test coverage.
    Punctuation,
}

fn word_class(ch: char) -> WordClass {
    if ch.is_whitespace() {
        WordClass::Whitespace
    } else if ch.is_alphanumeric() || ch == '_' {
        WordClass::Word
    } else {
        WordClass::Punctuation
    }
}

/// One open file's real, live-edited text plus real cursor/selection/IME-composition state - see
/// this module's own docs. `selected_range`/`marked_range` are byte offsets into [`Self::content`]
/// (never UTF-16 code units - that conversion happens only at `crate::code_surface::editing`'s
/// `EntityInputHandler` boundary, exactly like `vendor/zed/crates/gpui/examples/input.rs`'s own
/// `TextInput` keeps its `selected_range` in UTF-8 bytes throughout).
#[derive(Debug, Clone)]
pub struct EditBuffer {
    /// The real absolute filesystem path this buffer will be written to by an explicit save -
    /// see `crate::root::AdeApp::edit_buffers`' own docs for how this differs from that map's key
    /// (a worktree-relative path, matching `AdeApp::open_files`' own convention).
    pub path: PathBuf,
    /// The live, edited text - the real source of truth for what's on screen and what an
    /// explicit save writes. Never touched directly; every mutation goes through
    /// [`Self::replace_range`]/[`Self::replace_and_mark_range`].
    pub content: String,
    /// Owned (not `&'static str`, since it must outlive `content`'s own lifetime as a plain
    /// field, unlike `crate::language`'s registry, which only ever hands out `&'static str`
    /// borrows of its own static tables) - the extension `Self::highlighter` looks up.
    pub extension: Option<String>,
    /// A snapshot of [`Self::content`] at the moment of the last successful save, or at load
    /// time if never saved since - [`Self::is_dirty`]'s real comparison baseline.
    pub saved_content: String,
    /// The real on-disk mtime this buffer was seeded from (load time) or last wrote (save time) -
    /// `crate::code_surface::editing`'s external-change-conflict check compares a fresh
    /// `std::fs::metadata` read against this, not against `crate::root::AdeApp::file_view_cache`
    /// (which is throttled and serves a different purpose - see that field's own docs).
    pub saved_mtime: Option<SystemTime>,
    /// The real on-disk length paired with [`Self::saved_mtime`] - same real-metadata-at-load-or-
    /// save-time discipline as `code_view::ParsedFile::len`.
    pub saved_len: u64,
    /// The real, currently-displayed per-line syntax-highlighted content - reused directly by
    /// `crate::code_surface::file_view::render_file_view`'s row builder exactly like `code_view::ParsedFile::lines`
    /// is for the read-only path. See [`Self::highlight_dirty`] for when this is *plain* text
    /// (correct, just not yet syntax-colored) versus real `tree-sitter` output.
    pub lines: Vec<code_view::RenderedLine>,
    /// Byte ranges within [`Self::content`] for each of [`Self::lines`]' entries (index-aligned),
    /// excluding line-ending bytes - derived from the exact same real function
    /// (`code_view::line_ranges`) [`code_view::build_lines`] itself uses internally, so this
    /// buffer's own byte-offset<->line/column mapping can never disagree with what's actually
    /// displayed (see this module's own doc comment on why a second, independent line-splitter
    /// would risk a real CRLF off-by-one).
    pub line_ranges: Vec<Range<usize>>,
    /// The real cumulative UTF-16 length of [`Self::content`] up to (not including) each of
    /// [`Self::line_ranges`]' own entries (index-aligned) - i.e. `utf16_line_starts[i]` is
    /// `content[0..line_ranges[i].start]`'s own real UTF-16 length, counting every byte before
    /// that line (including every earlier line's own line-ending bytes). [`Self::offset_to_utf16`]/
    /// [`Self::offset_from_utf16`] binary-search this table to resolve straight to the *line*
    /// containing an offset, so the real per-character UTF-16 scan they still need only ever
    /// covers that one line's own text, never the whole buffer - see [`Self::splice_lines`]'s own
    /// docs for why a per-keystroke whole-buffer scan here was a real, measured performance bug.
    pub utf16_line_starts: Vec<usize>,
    /// `true` once a real `tree-sitter` re-highlight is owed - set by [`Self::rebuild_plain_full`]/
    /// [`Self::splice_lines`], cleared by [`Self::apply_highlight`]. See this module's own docs
    /// for why this exists.
    pub highlight_dirty: bool,
    /// The real cursor/selection - a caret when empty (`start == end`), a real selection
    /// otherwise. The "active" end (where the caret visually sits while shift-selecting) is
    /// [`Self::selection_reversed`]-dependent - see [`Self::cursor_offset`].
    pub selected_range: Range<usize>,
    /// `true` when the selection was extended leftward from its anchor (so [`Self::cursor_offset`]
    /// is [`Self::selected_range`]'s `start`, not its `end`) - mirrors
    /// `vendor/zed/crates/gpui/examples/input.rs`'s own `TextInput::selection_reversed` exactly.
    pub selection_reversed: bool,
    /// The IME composition range, if an input method is currently composing - `Some` only between
    /// a [`Self::replace_and_mark_range`] call and the matching [`Self::unmark`]/a plain
    /// [`Self::replace_range`] (which always clears it, matching a real IME commit).
    pub marked_range: Option<Range<usize>>,
    /// Up/Down's remembered target column (a byte-offset-within-line approximation - this app's
    /// File view is monospace-only, so a true visual-width-aware column isn't needed for a
    /// straight vertical caret; see `crate::code_surface::editing`'s own docs for this documented scope
    /// decision), preserved across a consecutive run of `Self::move_up`/`Self::move_down` and
    /// cleared by every other cursor-moving action - standard editor "sticky column" behavior.
    pub goal_column: Option<usize>,
    /// This buffer's real, multi-step undo/redo history (GitHub issue #17) - see this module's own
    /// docs. Private: every write goes through [`Self::record_edit`], so no call site can push a
    /// group that doesn't correspond to a splice that actually happened.
    history: TextHistory,
    /// `true` only while [`Self::undo`]/[`Self::redo`] is replaying a group, so the splices they
    /// perform aren't recorded as fresh edits - see this module's own docs.
    replaying: bool,
    /// Every real cursor *beyond* the primary (`Self::selected_range`/`Self::selection_reversed`),
    /// empty in ordinary single-cursor use (the only state a freshly constructed buffer is ever
    /// in). See this module's own "Multi-cursor" docs for the overall design and every method that
    /// populates/consumes this.
    pub secondary_cursors: Vec<SecondaryCursor>,
}

/// One additional cursor/selection beyond the primary `EditBuffer::selected_range`/
/// `EditBuffer::selection_reversed` - see `EditBuffer`'s own "Multi-cursor" docs. Never left
/// overlapping another real cursor (primary or secondary): every mutation that could produce a
/// collision re-merges through `EditBuffer::merge_colliding_cursors` immediately afterward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryCursor {
    /// Byte range into `EditBuffer::content` - a caret when empty, a real selection otherwise,
    /// exactly like the primary's own `EditBuffer::selected_range`.
    pub range: Range<usize>,
    /// Mirrors `EditBuffer::selection_reversed` for this one cursor - which end is "active" while
    /// a future shift-extend or occurrence-search reads it.
    pub reversed: bool,
}

impl EditBuffer {
    /// Real constructor: seeds `content`/`extension`/the real on-disk `mtime`/`len` this buffer
    /// was loaded from, and runs a real, immediate `tree-sitter` highlight
    /// (`code_view::highlighter_for_extension`/`code_view::build_lines`) - fine for a fresh
    /// buffer built directly (tests, or a small file), but see [`Self::from_highlighted`] for the
    /// production call site (`crate::root::AdeApp::spawn_file_load`), which avoids paying this
    /// cost a second time on the foreground thread when a background load already computed it.
    pub fn new(
        path: PathBuf,
        content: String,
        extension: Option<String>,
        mtime: Option<SystemTime>,
        len: u64,
    ) -> Self {
        let spans = match code_view::highlighter_for_extension(extension.as_deref()) {
            Some(highlighter) => highlighter(&content),
            None => Vec::new(),
        };
        let lines = code_view::build_lines(&content, &spans);
        Self::assemble(path, content, extension, lines, mtime, len)
    }

    /// Production constructor: takes already-highlighted `lines` (computed off the foreground
    /// thread by whichever background load also read `content`) rather than re-running the real
    /// highlighter here - see this module's own "Re-highlighting cost" docs for the measured
    /// reason a synchronous foreground `tree-sitter` parse of a large file must be avoided.
    pub fn from_highlighted(
        path: PathBuf,
        content: String,
        extension: Option<String>,
        lines: Vec<code_view::RenderedLine>,
        mtime: Option<SystemTime>,
        len: u64,
    ) -> Self {
        Self::assemble(path, content, extension, lines, mtime, len)
    }

    fn assemble(
        path: PathBuf,
        content: String,
        extension: Option<String>,
        lines: Vec<code_view::RenderedLine>,
        mtime: Option<SystemTime>,
        len: u64,
    ) -> Self {
        let line_ranges = code_view::line_ranges(&content);
        let utf16_line_starts = Self::cumulative_utf16_line_starts(&content, &line_ranges);
        Self {
            path,
            saved_content: content.clone(),
            content,
            extension,
            saved_mtime: mtime,
            saved_len: len,
            lines,
            line_ranges,
            utf16_line_starts,
            highlight_dirty: false,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            goal_column: None,
            history: TextHistory::new(),
            replaying: false,
            secondary_cursors: Vec::new(),
        }
    }

    /// The real, local (i.e. relative to `source`'s own start, not any wider buffer)
    /// per-line cumulative UTF-16 length table [`Self::utf16_line_starts`] itself holds -
    /// factored out so both [`Self::assemble`] (the whole buffer) and [`Self::splice_lines`]
    /// (just the narrow region an edit actually touches) build it the exact same real way,
    /// rather than two independent implementations that could silently disagree.
    fn cumulative_utf16_line_starts(source: &str, line_ranges: &[Range<usize>]) -> Vec<usize> {
        let mut starts = Vec::with_capacity(line_ranges.len());
        let mut byte_cursor = 0usize;
        let mut utf16_cursor = 0usize;
        for range in line_ranges {
            if range.start > byte_cursor {
                utf16_cursor += source[byte_cursor..range.start]
                    .chars()
                    .map(char::len_utf16)
                    .sum::<usize>();
            }
            starts.push(utf16_cursor);
            utf16_cursor += source[range.clone()]
                .chars()
                .map(char::len_utf16)
                .sum::<usize>();
            byte_cursor = range.end;
        }
        starts
    }

    /// The real highlighter this buffer's `extension` resolves to, per `crate::language`'s
    /// canonical registry (see `code_view::highlighter_for_extension`'s own docs) - read by
    /// `crate::root::AdeApp`'s debounced background re-highlight task.
    pub fn highlighter(&self) -> Option<HighlighterFn> {
        code_view::highlighter_for_extension(self.extension.as_deref())
    }

    /// `true` iff [`Self::content`] has changed since [`Self::saved_content`] was last captured
    /// (load time, or the last successful save) - the File view's real dirty-state indicator.
    pub fn is_dirty(&self) -> bool {
        self.content != self.saved_content
    }

    /// Records a successful save: `saved_content` becomes `written_content` - the *real* snapshot
    /// that was actually written to disk (captured by the caller at write-dispatch time), **not**
    /// `self.content` read fresh here. That distinction matters: if the user keeps typing while a
    /// background write is still in flight, `self.content` may already have moved past what's on
    /// disk by the time this runs - using it here would wrongly mark those newer, real unsaved
    /// keystrokes as saved. `mtime`/`len` are the real on-disk values observed right after the
    /// write (see `crate::code_surface::editing::AdeApp::spawn_file_save_loop`'s docs for why this is read
    /// fresh via `std::fs::metadata` after the write completes, not assumed from the write call
    /// succeeding).
    pub fn mark_saved(&mut self, written_content: String, mtime: Option<SystemTime>, len: u64) {
        self.saved_content = written_content;
        self.saved_mtime = mtime;
        self.saved_len = len;
    }

    /// Re-derives [`Self::lines`]/[`Self::line_ranges`]/[`Self::utf16_line_starts`] from the
    /// *whole* current [`Self::content`] with **no** real syntax highlighting (every run is plain
    /// [`code_view::HighlightKind::Text`]) - see this module's own "Re-highlighting cost" docs for
    /// why plain text, not a real highlight, is the right immediate result. Sets
    /// [`Self::highlight_dirty`].
    ///
    /// This is the *whole-buffer* path - real, still correct, still used both for
    /// [`Self::assemble`]'s own initial construction (there is no smaller "region" to derive from
    /// yet) and as [`Self::splice_lines`]'s own defensive fallback, but no longer the one every
    /// real keystroke pays for - see that method's own docs for the real, measured per-keystroke
    /// cost this was until this fix (a second, sibling instance of the same "whole-buffer
    /// recomputation on every keystroke" bug class this file's own `Self::previous_boundary`
    /// docs already describe fixing once).
    fn rebuild_plain_full(&mut self) {
        self.line_ranges = code_view::line_ranges(&self.content);
        self.lines = code_view::build_lines(&self.content, &[]);
        self.utf16_line_starts =
            Self::cumulative_utf16_line_starts(&self.content, &self.line_ranges);
        self.highlight_dirty = true;
    }

    /// Splices `new_text` into `self.content` at `range` (byte offsets into the *old* content,
    /// already char-boundary-clamped by every real caller - see [`Self::clamp_range`]) and
    /// incrementally patches [`Self::lines`]/[`Self::line_ranges`]/[`Self::utf16_line_starts`] to
    /// match, instead of [`Self::rebuild_plain_full`]'s whole-buffer recomputation - the real fix
    /// for a measured, second sibling instance of the "recompute the whole buffer on every
    /// keystroke" bug class this file's own [`Self::previous_boundary`] docs describe fixing once
    /// already (an audit measured this specific whole-buffer cost at ~0.65ms/keystroke, release,
    /// on this repo's own then-211KB `root/code_surface.rs`, since split into this folder).
    ///
    /// Only the line(s) `range` actually intersects are re-split, via `code_view::line_ranges`/
    /// `code_view::build_lines`, restricted to just that narrow substring - never the whole
    /// buffer. Every line entirely before the edit keeps its own already-correct entry untouched;
    /// every line entirely after it also keeps its own already-correct *text*, just with its
    /// [`Self::line_ranges`]/[`Self::utf16_line_starts`] entries shifted by a real, computable
    /// constant delta (`new_text.len()` minus the old range's own byte length; the equivalent in
    /// UTF-16 units for the latter table) rather than re-derived from scratch.
    ///
    /// Defensive by construction, not just by intent: every arithmetic step below is checked
    /// before it's trusted (line-index bounds, byte-range ordering, real UTF-8 char-boundary
    /// safety), and [`Self::rebuild_plain_full`] (the previous, always-correct whole-buffer path)
    /// is the real fallback the instant any of those checks doesn't hold. That fallback is never
    /// expected to actually fire for a real edit (the reasoning: [`Self::line_ranges`]' entries
    /// are always real char-boundary-safe line starts, `range` is always already clamped to a
    /// real char boundary by every caller, and plain arithmetic translation of an unmodified,
    /// untouched region's own boundary can't newly break that), but correctness always wins over
    /// cleverness here - a real, deliberately-defensive design, not a hidden gap.
    fn splice_lines(&mut self, range: Range<usize>, new_text: &str) {
        let byte_delta = new_text.len() as isize - (range.end - range.start) as isize;
        let (first_line, _) = self.line_col_for_offset(range.start);
        let (last_line, _) = self.line_col_for_offset(range.end);
        let old_content_len = self.content.len();

        let region = (first_line <= last_line && last_line < self.line_ranges.len()).then(|| {
            let region_start = self.line_ranges[first_line].start;
            let old_region_end = self
                .line_ranges
                .get(last_line + 1)
                .map(|next| next.start)
                .unwrap_or(old_content_len);
            // If the region reaches the *old* buffer's own real end, every remaining old
            // line entry is being replaced too - including [`code_view::line_ranges`]' own
            // final "phantom" empty entry representing the (possibly now different) state
            // after the last real `\n`, which sits at exactly this same byte offset. Without
            // extending the splice's own upper index to cover it too, that stale phantom
            // entry would survive the splice below as a real, live duplicate trailing line -
            // a genuine bug an earlier version of this fix shipped with (caught by this
            // method's own differential regression test, below).
            let last_line = if old_region_end == old_content_len {
                self.line_ranges.len() - 1
            } else {
                last_line
            };
            (region_start, old_region_end, last_line)
        });
        let Some((region_start, old_region_end, last_line)) = region else {
            self.content.replace_range(range, new_text);
            self.rebuild_plain_full();
            return;
        };

        // Captured before the splice below - the real anchor/baseline `Self::utf16_line_starts`'
        // own incremental update needs, mirroring `region_start`/`old_region_end`'s byte-offset
        // role for `Self::line_ranges`.
        let old_region_utf16_len = self.content[region_start..old_region_end]
            .chars()
            .map(char::len_utf16)
            .sum::<usize>();
        let utf16_base = self.utf16_line_starts.get(first_line).copied().unwrap_or(0);

        self.content.replace_range(range, new_text);
        let new_len = self.content.len();

        let region_end_signed = old_region_end as isize + byte_delta;
        let region_end_valid = region_end_signed >= region_start as isize
            && region_end_signed <= new_len as isize
            && self.content.is_char_boundary(region_start)
            && self
                .content
                .is_char_boundary(region_end_signed.max(0) as usize);
        if !region_end_valid {
            self.rebuild_plain_full();
            return;
        }
        let region_end = region_end_signed as usize;
        let Some(region_source) = self.content.get(region_start..region_end) else {
            self.rebuild_plain_full();
            return;
        };

        let local_ranges = code_view::line_ranges(region_source);
        let mut fresh_utf16_starts: Vec<usize> =
            Self::cumulative_utf16_line_starts(region_source, &local_ranges)
                .into_iter()
                .map(|local| local + utf16_base)
                .collect();
        let mut fresh_lines = code_view::build_lines(region_source, &[]);
        let mut fresh_ranges = local_ranges;

        let reaches_buffer_end = region_end == new_len;
        if !reaches_buffer_end {
            // `code_view::line_ranges`' own "the line after the last `\n`" convention is correct
            // only when that `\n` really is the buffer's own last byte; here it's instead the
            // boundary into the next, untouched real line (kept as-is, just shifted below), so
            // this substring's own trailing empty entry is spurious - it must not become a real,
            // phantom blank line spliced into the middle of the buffer.
            fresh_ranges.pop();
            fresh_lines.pop();
            fresh_utf16_starts.pop();
        }
        if fresh_ranges.is_empty() {
            // Never expected (a non-buffer-end region always ends in a real `\n`, so
            // `code_view::line_ranges` always returns at least one real line plus the spurious
            // trailing one popped above) - the same defensive fallback discipline as above.
            self.rebuild_plain_full();
            return;
        }
        for local_range in &mut fresh_ranges {
            local_range.start += region_start;
            local_range.end += region_start;
        }

        let new_region_utf16_len = region_source.chars().map(char::len_utf16).sum::<usize>();
        let utf16_delta = new_region_utf16_len as isize - old_region_utf16_len as isize;

        let inserted = fresh_ranges.len();
        self.lines.splice(first_line..=last_line, fresh_lines);
        self.line_ranges
            .splice(first_line..=last_line, fresh_ranges);
        self.utf16_line_starts
            .splice(first_line..=last_line, fresh_utf16_starts);

        for r in self.line_ranges.iter_mut().skip(first_line + inserted) {
            r.start = (r.start as isize + byte_delta) as usize;
            r.end = (r.end as isize + byte_delta) as usize;
        }
        for u in self
            .utf16_line_starts
            .iter_mut()
            .skip(first_line + inserted)
        {
            *u = (*u as isize + utf16_delta) as usize;
        }

        self.highlight_dirty = true;
    }

    /// Installs a real, freshly-computed highlight result - but only if `content_snapshot`
    /// (whatever [`Self::content`] was when the background highlight was *started*) still matches
    /// [`Self::content`] now. A mismatch means a further edit landed while the highlight was
    /// computing; discarding it (rather than applying stale-position colors to different text) is
    /// the real, honest choice - `highlight_dirty` stays `true`, so the caller's next debounce
    /// tick tries again against the newer content. Returns whether it was applied.
    pub fn apply_highlight(
        &mut self,
        content_snapshot: &str,
        lines: Vec<code_view::RenderedLine>,
    ) -> bool {
        if self.content != content_snapshot {
            return false;
        }
        self.lines = lines;
        self.highlight_dirty = false;
        true
    }

    /// The real "active" end of the selection - where the caret visually sits. Equal to
    /// `selected_range.end` normally, or `.start` while shift-selecting leftward
    /// ([`Self::selection_reversed`]).
    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn floor_char_boundary(&self, offset: usize) -> usize {
        let len = self.content.len();
        if offset >= len {
            return len;
        }
        let mut offset = offset;
        while offset > 0 && !self.content.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    /// Clamps an externally-supplied range (IME/UTF-16-derived, or a mouse click's hit-tested
    /// byte offset) into real bounds on a real char boundary - defensive, so a bad input can
    /// never panic `String::replace_range`'s own char-boundary requirement.
    fn clamp_range(&self, range: Range<usize>) -> Range<usize> {
        let len = self.content.len();
        let mut start = range.start.min(len);
        let mut end = range.end.min(len);
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        self.floor_char_boundary(start)..self.floor_char_boundary(end)
    }

    /// Moves the caret to `offset` with no selection - the real target of `Left`/`Right`/`Home`/
    /// `End`/a plain click. Does **not** touch [`Self::goal_column`]; callers that mean this as a
    /// horizontal move clear it themselves (vertical moves deliberately preserve it across a
    /// consecutive run - see [`Self::move_up`]/[`Self::move_down`]).
    ///
    /// Also clears [`Self::secondary_cursors`] - see this module's own "Multi-cursor" docs for why
    /// an ordinary, unmodified move/click always collapses back to a single real cursor. Every
    /// per-cursor call this method receives from [`Self::move_every_cursor`]'s own internal loop
    /// (the mechanism that moves every real cursor together for a plain arrow key) always finds
    /// this already empty by construction at that point (see that method's own docs), so this is
    /// never a lossy no-op there.
    pub fn move_to(&mut self, offset: usize) {
        self.secondary_cursors.clear();
        let offset = self.floor_char_boundary(offset.min(self.content.len()));
        self.selected_range = offset..offset;
        self.selection_reversed = false;
    }

    /// Extends the selection to `offset` from whichever end is currently anchored, flipping
    /// [`Self::selection_reversed`] if the selection crosses over itself - ported from
    /// `vendor/zed/crates/gpui/examples/input.rs`'s own `TextInput::select_to`.
    ///
    /// Also clears [`Self::secondary_cursors`] - see [`Self::move_to`]'s own docs for why, and for
    /// why this is never lossy when called from within [`Self::move_every_cursor`]'s own loop.
    pub fn select_to(&mut self, offset: usize) {
        self.secondary_cursors.clear();
        let offset = self.floor_char_boundary(offset.min(self.content.len()));
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
    }

    /// Real grapheme-cluster-aware boundary just before `offset` - based on
    /// `vendor/zed/crates/gpui/examples/input.rs`'s own `TextInput::previous_boundary`, but
    /// **not** ported verbatim: an early version of this method scanned `unicode-segmentation`
    /// over this buffer's *whole* `content` (matching the example, whose own `TextInput` is
    /// single-line and has no "whole buffer" vs. "one line" distinction to matter for) - a real,
    /// measured performance bug fixed here, since this buffer's content can be up to
    /// `code_view::MAX_FILE_BYTES` (2 MiB): measured at ~1.8ms/call on this repo's own
    /// then-~200 KiB `root/code_surface.rs` (since split into this folder) in a release
    /// build, i.e. up to tens of milliseconds per single
    /// arrow-key/Backspace/Delete press on a large real file - the exact "expensive work on
    /// every keystroke" bug class this project's own history keeps finding. Grapheme boundaries
    /// never span a line break (a `\n`, or a `\r\n` pair `unicode-segmentation` treats as one
    /// cluster per UAX #29, is always its own hard break under UAX #29's control-character
    /// rules), so scanning only [`Self::line_ranges`]' current line's own text - `O(line
    /// length)`, not `O(whole buffer)` - finds an identical real boundary to the old whole-buffer
    /// scan. Crossing a line boundary (`offset` already at/before this line's own start) is
    /// handled explicitly rather than by scanning into the newline bytes at all: the previous
    /// real boundary is exactly the previous line's own real end (its last real byte, before its
    /// own trailing line-ending bytes), read directly off [`Self::line_ranges`].
    pub fn previous_boundary(&self, offset: usize) -> usize {
        let (line, _) = self.line_col_for_offset(offset);
        let Some(line_range) = self.line_ranges.get(line) else {
            return 0;
        };
        if offset <= line_range.start {
            return if line == 0 {
                0
            } else {
                self.line_ranges[line - 1].end
            };
        }
        let local_offset = offset - line_range.start;
        self.content[line_range.clone()]
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < local_offset).then_some(line_range.start + index))
            .unwrap_or(line_range.start)
    }

    /// The mirror of [`Self::previous_boundary`] - see its own docs for why this is a real,
    /// line-scoped scan (not a whole-buffer one) and for the measured performance bug that fixed.
    pub fn next_boundary(&self, offset: usize) -> usize {
        let (line, _) = self.line_col_for_offset(offset);
        let Some(line_range) = self.line_ranges.get(line) else {
            return self.content.len();
        };
        if offset >= line_range.end {
            return self
                .line_ranges
                .get(line + 1)
                .map(|next| next.start)
                .unwrap_or(self.content.len());
        }
        let local_offset = offset - line_range.start;
        self.content[line_range.clone()]
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > local_offset).then_some(line_range.start + index))
            .unwrap_or(line_range.end)
    }

    /// Real UTF-16->UTF-8 offset conversion, needed because GPUI's real IME/input-method protocol
    /// addresses text in UTF-16 code units (`crate::code_surface::editing`'s `EntityInputHandler` impl is
    /// the only real caller; every other method on this type stays in UTF-8 bytes) - ported in
    /// spirit from `vendor/zed/crates/gpui/examples/input.rs`, but no longer a whole-buffer scan:
    /// a real, measured performance bug an audit caught (~122µs/call, release, on this repo's
    /// own then-211KB `root/code_surface.rs`, since split into this folder - a real cost paid
    /// up to twice per real platform IME keystroke,
    /// per `vendor/zed/crates/gpui_linux/src/linux/x11/window.rs:1233`'s own `selected_text_range`
    /// call site) - the exact same "whole-buffer work on every keystroke" bug class this file's
    /// own `Self::previous_boundary`/`Self::splice_lines` docs describe fixing elsewhere. Fixed
    /// the same way: [`Self::utf16_line_starts`] binary-searches straight to the one real *line*
    /// `offset` falls in (`O(log n)`), so the actual per-character scan below only ever covers
    /// that line's own text, never the whole buffer.
    pub fn offset_from_utf16(&self, offset: usize) -> usize {
        if self.utf16_line_starts.is_empty() {
            return 0;
        }
        // The last real line whose own cumulative start is `<= offset` - `partition_point`, not
        // `binary_search`, since an empty line contributes zero UTF-16 length, so
        // `utf16_line_starts` can hold real, adjacent duplicate values a plain `binary_search`
        // isn't guaranteed to resolve to the rightmost (i.e. real, intended) match.
        let line = self
            .utf16_line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let Some(range) = self.line_ranges.get(line) else {
            return self.content.len();
        };
        let base = self.utf16_line_starts[line];
        let target_local_utf16 = offset.saturating_sub(base);

        let mut utf16_count = 0usize;
        let mut byte_count = 0usize;
        for ch in self.content[range.clone()].chars() {
            if utf16_count >= target_local_utf16 {
                break;
            }
            utf16_count += ch.len_utf16();
            byte_count += ch.len_utf8();
        }
        range.start + byte_count
    }

    /// The mirror of [`Self::offset_from_utf16`] - see its own docs for the real, measured
    /// whole-buffer-scan performance bug this fix removes the same way (binary-searching
    /// [`Self::line_ranges`]/[`Self::utf16_line_starts`] straight to `offset`'s own line via
    /// [`Self::line_col_for_offset`], then scanning only that line's own text).
    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        let (line, col) = self.line_col_for_offset(offset);
        let base = self.utf16_line_starts.get(line).copied().unwrap_or(0);
        let Some(range) = self.line_ranges.get(line) else {
            return base;
        };
        let Some(local_text) = self.content.get(range.start..range.start + col) else {
            return base;
        };
        base + local_text.chars().map(char::len_utf16).sum::<usize>()
    }

    /// Real byte offset in [`Self::content`] for an LSP `Position` (a 0-based line plus a
    /// UTF-16 code-unit `character` offset within it) - the inverse mapping Revision R8.5b's
    /// real Completions popup needs to turn a real completion item's own `text_edit` range (or a
    /// real diagnostic's range, in principle, though nothing here uses it for that yet) into a
    /// real buffer splice, via `crate::lsp::completion_popup`. An out-of-range `line` clamps to the
    /// real last line (mirroring [`Self::offset_for_line_col`]'s own clamping) rather than
    /// panicking on a language server's own response; [`Self::offset_from_utf16`]'s own
    /// defensive per-line clamping (see its docs) then bounds `utf16_character` too, so this can
    /// never walk past real buffer content even for a wildly out-of-range value.
    pub fn offset_for_position(&self, line: u32, utf16_character: u32) -> usize {
        let line = (line as usize).min(self.utf16_line_starts.len().saturating_sub(1));
        let utf16_base = self.utf16_line_starts.get(line).copied().unwrap_or(0);
        self.offset_from_utf16(utf16_base + utf16_character as usize)
    }

    pub fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    /// Splices `new_text` into `range` (or, when `range` is `None`, [`Self::marked_range`] if an
    /// IME composition is active, else the current [`Self::selected_range`] - the same real
    /// three-way priority `EntityInputHandler::replace_text_in_range`'s own contract describes),
    /// moves the caret to just after the inserted text, and clears [`Self::marked_range`] (a real
    /// IME commit, or an ordinary keystroke/paste/Backspace/Delete/Enter, all end composition).
    /// The one real splice path every text-changing action reduces to - see this module's own
    /// top docs.
    pub fn replace_range(&mut self, range: Option<Range<usize>>, new_text: &str) {
        // Multi-cursor: an *implicit* range (typing/paste/an ordinary Backspace-or-Delete-driven
        // call that already resolved its own explicit range below instead - see those methods'
        // own bodies) applies `new_text` at every real cursor at once, not just the primary - see
        // this module's own "Multi-cursor" docs for why an *explicit* `range` deliberately still
        // takes the single-cursor path unconditionally (the documented IME/completion gap).
        if range.is_none() && !self.secondary_cursors.is_empty() {
            let new_text = new_text.to_string();
            self.apply_at_every_cursor(
                EditKind::for_replacement(&new_text),
                |buffer, cursor_range| (buffer.clamp_range(cursor_range), new_text.clone()),
            );
            return;
        }
        self.replace_range_recording(range, new_text, None, None);
    }

    /// [`Self::replace_range`] with explicit history context - the real implementation both it and
    /// the caret-preserving deletion helpers ([`Self::backspace`]/[`Self::delete_forward`]) share.
    ///
    /// `before` overrides the selection recorded as "where this edit started". That override is
    /// load-bearing, not cosmetic: `backspace` extends the selection over the grapheme it is about
    /// to delete *before* splicing, so the selection this method would otherwise observe is
    /// already the doomed range rather than the caret the user actually had. Recording that would
    /// (a) restore a selection the user never made when undone, and (b) break coalescing outright,
    /// since consecutive backspaces would each report a different `before` than the previous one's
    /// `after` and so never group - see `crate::text_history`'s own caret-continuity rule.
    ///
    /// `kind` overrides the inferred [`EditKind`], for a caller that knows an edit is a paste, an
    /// accepted completion, or an external reload rather than ordinary typing.
    fn replace_range_recording(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        before: Option<SelectionSnapshot>,
        kind: Option<EditKind>,
    ) {
        let range = range
            .map(|range| self.clamp_range(range))
            .or_else(|| self.marked_range.clone())
            // Defensively clamped too - see this method's own docs on why `self.selected_range`
            // itself is never trusted blindly as a splice target, however it got here.
            .unwrap_or_else(|| self.clamp_range(self.selected_range.clone()));
        // An edit that lands while a live IME composition is active is part of that composition's
        // own single atomic step (the platform commits a composition by replacing the marked range
        // through this exact path), so it must record with the composition's own kind rather than
        // starting a fresh typing group.
        let was_composing = self.marked_range.is_some();
        let before = before.unwrap_or_else(|| self.selection_snapshot());
        let removed = self
            .content
            .get(range.clone())
            .map(|text| text.to_string())
            .unwrap_or_default();
        self.splice_lines(range.clone(), new_text);
        let new_pos = range.start + new_text.len();
        self.selected_range = new_pos..new_pos;
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_column = None;
        let kind = match (kind, was_composing) {
            (Some(kind), _) => kind,
            (None, true) => EditKind::Ime,
            (None, false) => EditKind::for_replacement(new_text),
        };
        self.record_edit(range.start, removed, new_text.to_string(), before, kind);
        // A composition ends here only when this edit genuinely *committed* something. A
        // mid-composition Backspace also arrives through this path (`Self::backspace` -> here,
        // with the composition still live) and clears `marked_range` as a side effect of the
        // splice, but the platform routinely keeps composing afterwards and sends further
        // `setMarkedText` updates - sealing on that would split one real composition into two undo
        // steps, which this method did until self-review caught it. Deletions therefore leave the
        // group open: it stays `EditKind::Ime`, so only a continuing composition can rejoin it
        // (ordinary typing has a different kind and starts its own group regardless), and a
        // composition that really did end is still sealed by its own commit, by `Self::unmark`, or
        // by an emptied-out `replace_and_mark_range`.
        if was_composing && !new_text.is_empty() {
            self.history.seal();
        }
    }

    /// The IME composition variant of [`Self::replace_range`] - splices `new_text` the same way,
    /// but records it as [`Self::marked_range`] instead of committing it. `range` is a byte range
    /// (already converted from UTF-16 by the caller against this buffer's *pre-edit* content -
    /// safe, since it's resolved before `content` changes below).
    ///
    /// `new_selected_range_utf16` is the real platform IME protocol's own composing-caret/
    /// selection position - critically, a UTF-16 offset **relative to `new_text` itself** (the
    /// composing string), *not* an offset into the whole buffer. Verified for real against both
    /// real platform backends this app supports: macOS's `NSTextInputClient::setMarkedText:
    /// selectedRange:` (`vendor/zed/crates/gpui_macos/src/window.rs:2794,2809`) passes the
    /// selection *within the marked text* on every composition update; Windows
    /// (`vendor/zed/crates/gpui_windows/src/events.rs:731-745`) does the same via its own
    /// `comp_string`-relative `caret_pos`. An earlier version of this method got this real
    /// contract wrong - documented here as a "deliberate deviation" from `vendor/zed/crates/gpui/
    /// examples/input.rs`'s own reference `TextInput`, but that framing was itself the bug: both
    /// the reference's own formula and the old one here converted `new_selected_range_utf16` via
    /// [`Self::range_from_utf16`], which resolves a UTF-16 offset against [`Self::content`] - the
    /// *whole buffer*, starting from offset 0 - answering a completely different question than
    /// "where inside `new_text`". That corrupted [`Self::selected_range`] into whatever byte
    /// offset `new_selected_range_utf16`'s value happened to land on when misread as a
    /// whole-buffer offset, which can fall on a non-UTF-8-char-boundary byte - a real,
    /// live-reproduced panic on the very next [`Self::replace_range`]/[`Self::replace_and_mark_range`]
    /// call (`String::replace_range`'s own "start of range should be a character boundary"),
    /// reproduced with real Japanese IME composition input reporting a non-default composing
    /// caret position (the one real shape the old, `None`-only test coverage here could never
    /// have caught - see this file's own `mod tests` for the real regression that now covers it).
    /// Fixed by converting `new_selected_range_utf16` against `new_text` directly (never
    /// `self.content`), then rebasing onto `range.start` - the real, correct interpretation of
    /// "an offset within the composing text" both formulas above were actually trying to express.
    pub fn replace_and_mark_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
    ) {
        let range = range
            .map(|range| self.clamp_range(range))
            .or_else(|| self.marked_range.clone())
            // Defensively clamped too - see `Self::replace_range`'s own identical hardening.
            .unwrap_or_else(|| self.clamp_range(self.selected_range.clone()));
        let before = self.selection_snapshot();
        let removed = self
            .content
            .get(range.clone())
            .map(|text| text.to_string())
            .unwrap_or_default();
        self.splice_lines(range.clone(), new_text);
        self.marked_range = if new_text.is_empty() {
            None
        } else {
            Some(range.start..range.start + new_text.len())
        };
        self.selected_range = match &new_selected_range_utf16 {
            Some(range_utf16) => {
                // A real, local UTF-16-relative-to-`new_text` byte conversion - deliberately not
                // `Self::offset_from_utf16` (that converts against the whole `self.content`, the
                // exact real bug this method's own docs describe).
                let byte_offset_within_new_text = |utf16_target: usize| {
                    let (mut byte, mut utf16) = (0usize, 0usize);
                    for ch in new_text.chars() {
                        if utf16 >= utf16_target {
                            break;
                        }
                        utf16 += ch.len_utf16();
                        byte += ch.len_utf8();
                    }
                    byte
                };
                let start = range.start + byte_offset_within_new_text(range_utf16.start);
                let end = range.start + byte_offset_within_new_text(range_utf16.end);
                self.clamp_range(start..end)
            }
            None => {
                let end = range.start + new_text.len();
                end..end
            }
        };
        self.goal_column = None;
        self.record_edit(
            range.start,
            removed,
            new_text.to_string(),
            before,
            EditKind::Ime,
        );
        if self.marked_range.is_none() {
            // The composition ended by being emptied out (a cancelled/cleared composing string) -
            // a real, hard boundary, exactly like the commit path in
            // `Self::replace_range_recording`.
            self.history.seal();
        }
    }

    /// The real, current caret/selection as a history snapshot.
    fn selection_snapshot(&self) -> SelectionSnapshot {
        SelectionSnapshot::of(&self.selected_range, self.selection_reversed)
    }

    /// Pushes one already-performed splice into [`Self::history`]. Suppressed entirely while
    /// [`Self::replaying`] - see this module's own docs.
    fn record_edit(
        &mut self,
        at: usize,
        removed: String,
        inserted: String,
        before: SelectionSnapshot,
        kind: EditKind,
    ) {
        if self.replaying {
            return;
        }
        let after = self.selection_snapshot();
        self.history.record(
            TextEdit {
                at,
                removed,
                inserted,
            },
            before,
            after,
            kind,
            Instant::now(),
        );
    }

    /// Closes the current undo group, so the next edit always starts a fresh one. The caller-driven
    /// half of `crate::text_history`'s policy: paste, cut, an accepted completion and an external
    /// reload are all real group boundaries this type can't infer from the splice alone.
    pub fn seal_history(&mut self) {
        self.history.seal();
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Steps one real undo group back: inverts every edit in it (in reverse order - the only order
    /// that is correct once a group holds more than one edit, which is exactly what one
    /// multi-cursor edit will be) and restores the recorded caret **and** selection.
    ///
    /// Returns whether anything was undone. A group whose recorded `inserted` text doesn't match
    /// what's actually in `content` right now is refused outright, leaving both the buffer and the
    /// history cursor exactly where they were: that can only mean the history has desynchronized
    /// from the content, and applying it anyway would silently corrupt real, unsaved user text.
    /// The refusal is defensive, not expected - every content mutation on this type records - and
    /// is covered by a real test that deliberately desynchronizes the two.
    ///
    /// The peek/validate/apply/commit ordering is what makes "leaving the cursor exactly where it
    /// was" true rather than merely intended - see `crate::text_history::TextHistory::peek_undo`'s
    /// own docs for the desynchronization a combined `undo()` would have left behind on refusal.
    pub fn undo(&mut self) -> bool {
        let Some(group) = self.history.peek_undo() else {
            return false;
        };
        if !self.replay_is_safe(&group.edits, false) {
            return false;
        }
        self.replaying = true;
        for edit in group.edits.iter().rev() {
            self.splice_lines(edit.new_range(), &edit.removed);
        }
        self.replaying = false;
        self.restore_selection(group.before);
        self.history.commit_undo();
        true
    }

    /// The mirror of [`Self::undo`] - replays a group forward, in order, and restores its `after`
    /// selection.
    pub fn redo(&mut self) -> bool {
        let Some(group) = self.history.peek_redo() else {
            return false;
        };
        if !self.replay_is_safe(&group.edits, true) {
            return false;
        }
        self.replaying = true;
        for edit in &group.edits {
            self.splice_lines(edit.old_range(), &edit.inserted);
        }
        self.replaying = false;
        self.restore_selection(group.after);
        self.history.commit_redo();
        true
    }

    /// Whether replaying `edits` (forward when `forward`, inverted otherwise) really describes this
    /// buffer's current bytes at every step - checked against a scratch copy of `content` before a
    /// single byte of the real buffer is touched, so a refusal is genuinely all-or-nothing rather
    /// than a half-applied group.
    fn replay_is_safe(&self, edits: &[TextEdit], forward: bool) -> bool {
        let mut scratch = self.content.clone();
        if forward {
            edits
                .iter()
                .all(|edit| text_history::apply_forward(&mut scratch, edit))
        } else {
            edits
                .iter()
                .rev()
                .all(|edit| text_history::apply_inverse(&mut scratch, edit))
        }
    }

    fn restore_selection(&mut self, snapshot: SelectionSnapshot) {
        self.selected_range = self.clamp_range(snapshot.range());
        self.selection_reversed = snapshot.reversed;
        self.marked_range = None;
        self.goal_column = None;
        // Multi-cursor (issue #28): undo/redo only ever restores the primary's own snapshot (see
        // this module's own "Multi-cursor edits are one real undo step" docs for why) - clearing
        // every secondary here makes that real, not just documented: the buffer genuinely lands in
        // ordinary single-cursor state after an undo/redo, never a stale, unrestored secondary
        // left pointing at text that may no longer mean what it did before the step was undone.
        self.secondary_cursors.clear();
    }

    /// Adopts `new_content` (what a real, external writer - an agent CLI, a formatter, another
    /// editor - has just put on disk) as **one single undoable step**, rather than throwing this
    /// buffer's history away and starting over.
    ///
    /// That distinction is the whole point per GitHub issue #17: an external rewrite landing
    /// mid-session must never be a silent history wipe. It is recorded as a sealed, programmatic
    /// group, so the step before it and the step after it are both still reachable, and Ctrl+Z
    /// immediately after the reload really does put the pre-reload content back.
    ///
    /// The caret is preserved by byte offset, clamped into the new content - honest and cheap. It
    /// is deliberately *not* re-anchored by a diff of old against new (which would be the only way
    /// to keep it on "the same" line through a large rewrite): that is real work with real failure
    /// modes for a case where the content under the caret may not exist at all any more.
    ///
    /// Returns `false` when the content is already identical, having recorded nothing at all - a
    /// reload that changes nothing must not push an empty step the user has to press Ctrl+Z past.
    /// (It still refreshes the real on-disk `mtime`/`len` in that case; see the branch's own
    /// comment for why leaving those stale would re-report a change that isn't there.)
    pub fn reload_from_disk(
        &mut self,
        new_content: String,
        lines: Vec<code_view::RenderedLine>,
        mtime: Option<SystemTime>,
        len: u64,
    ) -> bool {
        if self.content == new_content {
            // Still refresh the real on-disk identity: the bytes match, so this buffer is
            // genuinely clean against the new file, and leaving a stale mtime/len behind would
            // make the next freshness check re-report a change that isn't there.
            self.saved_content = new_content;
            self.saved_mtime = mtime;
            self.saved_len = len;
            return false;
        }
        let before = self.selection_snapshot();
        let old_content = std::mem::replace(&mut self.content, new_content);
        let caret = self.floor_char_boundary(before.start.min(self.content.len()));
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_column = None;
        // Multi-cursor (issue #28): an external rewrite's own byte offsets have no reliable
        // relationship to whatever a secondary cursor was pointing at before it landed - the same
        // honest reasoning that already keeps this method from trying to diff-preserve the primary
        // caret's exact line, just applied to every cursor beyond it. Cleared rather than clamped
        // into now-possibly-nonsensical positions.
        self.secondary_cursors.clear();
        self.line_ranges = code_view::line_ranges(&self.content);
        self.utf16_line_starts =
            Self::cumulative_utf16_line_starts(&self.content, &self.line_ranges);
        self.lines = lines;
        self.highlight_dirty = false;
        self.saved_content = self.content.clone();
        self.saved_mtime = mtime;
        self.saved_len = len;
        let after = self.selection_snapshot();
        self.history
            .record_replacement(&old_content, &self.content, before, after, Instant::now());
        true
    }

    /// Ends an IME composition without committing it as a real edit (a cancelled/dismissed
    /// composition) - the marked text itself was already spliced into `content` by whichever
    /// [`Self::replace_and_mark_range`] call is being unmarked, so this only clears the marker.
    pub fn unmark(&mut self) {
        let was_composing = self.marked_range.is_some();
        self.marked_range = None;
        if was_composing {
            // The composition is over - a real, hard undo-group boundary, so whatever the user
            // types next is its own step rather than being absorbed into the composition's.
            self.history.seal();
        }
    }

    /// `Backspace`: deletes the grapheme before the caret, or the real selection if one exists.
    /// A no-op at the very start of the buffer (`self.previous_boundary` can't move further).
    ///
    /// Passes the real, just-computed one-grapheme (or real existing) [`Self::selected_range`]
    /// to [`Self::replace_range`] *explicitly*, rather than `None` - a real, user-visible bug an
    /// earlier version of this method had: `Self::replace_range`'s own real priority order
    /// (matching `EntityInputHandler::replace_text_in_range`'s documented contract) prefers
    /// [`Self::marked_range`] over [`Self::selected_range`] whenever both exist, so passing `None`
    /// while a real IME composition happens to be active deleted the *entire* composing text
    /// instead of one real grapheme - wrong every time Backspace was pressed mid-composition, a
    /// real, live-reachable case for any real CJK/composed input session.
    pub fn backspace(&mut self) {
        if !self.secondary_cursors.is_empty() {
            self.apply_at_every_cursor(EditKind::Delete, |buffer, cursor_range| {
                if cursor_range.is_empty() {
                    let previous = buffer.previous_boundary(cursor_range.start);
                    (previous..cursor_range.end, String::new())
                } else {
                    (cursor_range, String::new())
                }
            });
            return;
        }
        // Captured *before* the `select_to` below - see `Self::replace_range_recording`'s own docs
        // for why recording the post-`select_to` selection would both restore a selection the user
        // never made and silently defeat backspace coalescing.
        let before = self.selection_snapshot();
        if self.selected_range.is_empty() {
            let previous = self.previous_boundary(self.cursor_offset());
            if previous == self.cursor_offset() {
                return;
            }
            self.select_to(previous);
        }
        self.replace_range_recording(Some(self.selected_range.clone()), "", Some(before), None);
    }

    /// `Delete`: the mirror of [`Self::backspace`] - see its own docs for why the real, just-
    /// computed [`Self::selected_range`] is passed explicitly rather than `None`.
    pub fn delete_forward(&mut self) {
        if !self.secondary_cursors.is_empty() {
            self.apply_at_every_cursor(EditKind::Delete, |buffer, cursor_range| {
                if cursor_range.is_empty() {
                    let next = buffer.next_boundary(cursor_range.start);
                    (cursor_range.start..next, String::new())
                } else {
                    (cursor_range, String::new())
                }
            });
            return;
        }
        // See `Self::backspace`'s own docs for why this is captured before `select_to`.
        let before = self.selection_snapshot();
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if next == self.cursor_offset() {
                return;
            }
            self.select_to(next);
        }
        self.replace_range_recording(Some(self.selected_range.clone()), "", Some(before), None);
    }

    /// `Left`: collapses a real selection to its start, or moves the caret one real grapheme
    /// left. Clears [`Self::goal_column`] (a horizontal move). Moves every real cursor together
    /// when more than one is active - see [`Self::move_every_cursor`]'s own docs.
    pub fn move_left(&mut self) {
        self.move_every_cursor(Self::move_left_one_cursor);
    }

    fn move_left_one_cursor(&mut self) {
        self.goal_column = None;
        if self.selected_range.is_empty() {
            let previous = self.previous_boundary(self.cursor_offset());
            self.move_to(previous);
        } else {
            self.move_to(self.selected_range.start);
        }
    }

    /// The mirror of [`Self::move_left`].
    pub fn move_right(&mut self) {
        self.move_every_cursor(Self::move_right_one_cursor);
    }

    fn move_right_one_cursor(&mut self) {
        self.goal_column = None;
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            self.move_to(next);
        } else {
            self.move_to(self.selected_range.end);
        }
    }

    /// `Shift+Left`: extends the selection one real grapheme left, at every real cursor at once.
    pub fn select_left(&mut self) {
        self.move_every_cursor(Self::select_left_one_cursor);
    }

    fn select_left_one_cursor(&mut self) {
        self.goal_column = None;
        let previous = self.previous_boundary(self.cursor_offset());
        self.select_to(previous);
    }

    /// `Shift+Right`: extends the selection one real grapheme right, at every real cursor at once.
    pub fn select_right(&mut self) {
        self.move_every_cursor(Self::select_right_one_cursor);
    }

    fn select_right_one_cursor(&mut self) {
        self.goal_column = None;
        let next = self.next_boundary(self.cursor_offset());
        self.select_to(next);
    }

    /// `Ctrl/Cmd+A`: selects the whole buffer - always collapses to exactly one selection
    /// regardless of how many real cursors were active beforehand (`Self::move_to`/
    /// `Self::select_to` below already clear [`Self::secondary_cursors`] as their own first step -
    /// see those methods' own docs - so "select everything" never leaves a stray secondary cursor
    /// sitting inside the new, whole-buffer selection).
    pub fn select_all(&mut self) {
        self.goal_column = None;
        self.move_to(0);
        self.select_to(self.content.len());
    }

    /// `Tab` (GitHub issue #26): with no selection, splices `indent_unit` in at the caret exactly
    /// like an ordinary keystroke (via [`Self::replace_range`]) - the caret ends up right after
    /// it, same as typing any other text. With a real selection (single- or multi-line),
    /// prepends `indent_unit` to the start of every line [`crate::code_surface::indent::
    /// touched_line_range`] says the selection touches, and re-expands the selection to keep
    /// covering the same real lines (including the newly-inserted indentation) rather than
    /// collapsing to a caret - see that function's own docs for the "selection ends at column 0"
    /// exclusion this shares with [`Self::dedent_lines`].
    ///
    /// Returns whether anything real actually changed - `false` only for the pathological empty
    /// `indent_unit` case (`crate::code_surface::indent::indent_unit` never actually produces
    /// one, but this stays honest rather than assuming).
    ///
    /// ## One atomic undo step, not N (GitHub issue #17 integration)
    ///
    /// The multi-line branch below splices every touched line directly via [`Self::splice_lines`]
    /// (bypassing [`Self::replace_range`]'s own automatic per-call history recording) and records
    /// the whole run as one [`TextHistory::record_group`] call instead - seven per-line calls to
    /// `replace_range` would each be recorded as its own [`EditKind::Programmatic`] group (that
    /// kind's own "never coalesces" rule means they'd never merge back into one), so a single Tab
    /// press over N lines would need N separate `Ctrl+Z` presses to undo. See that method's own
    /// docs for why the edits must be collected in exactly this bottom-to-top order for the
    /// resulting group to replay correctly in both directions.
    pub fn indent_lines(&mut self, indent_unit: &str) -> bool {
        if indent_unit.is_empty() {
            return false;
        }
        if self.selected_range.is_empty() {
            self.replace_range(Some(self.selected_range.clone()), indent_unit);
            return true;
        }

        let before = self.selection_snapshot();
        let sel_start = self.selected_range.start;
        let sel_end = self.selected_range.end;
        let reversed = self.selection_reversed;
        let (first_line, _) = self.line_col_for_offset(sel_start);
        let (end_line, end_col) = self.line_col_for_offset(sel_end);
        let touched = indent::touched_line_range(first_line, end_line, end_col);

        // Bottom-to-top: inserting at an earlier line's own start would shift every later line's
        // byte offsets out from under a not-yet-processed later insertion otherwise. Edits are
        // pushed in this same bottom-to-top order, matching how they were really applied.
        let mut edits = Vec::with_capacity(touched.len());
        for line in touched.clone().rev() {
            let Some(line_start) = self.line_ranges.get(line).map(|range| range.start) else {
                continue;
            };
            self.splice_lines(line_start..line_start, indent_unit);
            edits.push(TextEdit {
                at: line_start,
                removed: String::new(),
                inserted: indent_unit.to_string(),
            });
        }

        let touched_count = touched.len();
        let new_start = self
            .line_ranges
            .get(first_line)
            .map(|range| range.start)
            .unwrap_or(sel_start);
        let new_end = (sel_end + indent_unit.len() * touched_count).min(self.content.len());
        self.selected_range = new_start..new_end.max(new_start);
        self.selection_reversed = reversed;
        self.marked_range = None;
        self.goal_column = None;
        if !self.replaying {
            let after = self.selection_snapshot();
            self.history
                .record_group(edits, before, after, Instant::now());
        }
        true
    }

    /// `Shift+Tab` (GitHub issue #26): the mirror of [`Self::indent_lines`] - removes up to one
    /// real indent level ([`crate::code_surface::indent::leading_dedent_len`]) from the start of
    /// every line the (possibly empty/collapsed-caret) selection touches, a genuine no-op for any
    /// line already at column 0 (or with no real leading whitespace at all). Re-expands the
    /// selection to keep covering the same real lines, shifted left by however much was actually
    /// removed *at or before* each end - never past that line's own new start.
    ///
    /// Returns whether anything real actually changed (`false` when every touched line had no
    /// real leading whitespace to remove) - `crate::code_surface::editing`'s caller uses this to
    /// skip a pointless re-highlight/LSP-sync/scroll-sync for a genuine no-op keystroke.
    ///
    /// Records every real per-line removal as one atomic [`TextHistory::record_group`] step - see
    /// [`Self::indent_lines`]'s own "One atomic undo step, not N" docs for why.
    pub fn dedent_lines(&mut self, tab_width: u8) -> bool {
        let before = self.selection_snapshot();
        let sel_start = self.selected_range.start;
        let sel_end = self.selected_range.end;
        let reversed = self.selection_reversed;
        let (first_line, _) = self.line_col_for_offset(sel_start);
        let (end_line, end_col) = self.line_col_for_offset(sel_end);
        let touched = if self.selected_range.is_empty() {
            first_line..first_line + 1
        } else {
            indent::touched_line_range(first_line, end_line, end_col)
        };

        let removed: Vec<usize> = touched
            .clone()
            .map(|line| {
                let Some(range) = self.line_ranges.get(line) else {
                    return 0;
                };
                let Some(text) = self.content.get(range.clone()) else {
                    return 0;
                };
                indent::leading_dedent_len(text, tab_width)
            })
            .collect();
        if removed.iter().all(|&len| len == 0) {
            return false;
        }

        let mut edits = Vec::with_capacity(touched.len());
        for (offset, line) in touched.clone().enumerate().rev() {
            let remove = removed[offset];
            if remove == 0 {
                continue;
            }
            let Some(line_start) = self.line_ranges.get(line).map(|range| range.start) else {
                continue;
            };
            let remove_range = line_start..line_start + remove;
            let Some(removed_text) = self.content.get(remove_range.clone()) else {
                continue;
            };
            let removed_text = removed_text.to_string();
            self.splice_lines(remove_range, "");
            edits.push(TextEdit {
                at: line_start,
                removed: removed_text,
                inserted: String::new(),
            });
        }

        let total_removed: usize = removed.iter().sum();
        let new_start = self
            .line_ranges
            .get(first_line)
            .map(|range| range.start)
            .unwrap_or(sel_start);
        let new_end = sel_end.saturating_sub(total_removed).max(new_start);
        self.selected_range = new_start..new_end;
        self.selection_reversed = reversed;
        self.marked_range = None;
        self.goal_column = None;
        if !self.replaying {
            let after = self.selection_snapshot();
            self.history
                .record_group(edits, before, after, Instant::now());
        }
        true
    }

    /// Real, line-scoped word boundary just before `offset` (GitHub issue #27:
    /// "Ctrl+Shift+arrows (word-wise)") - a maximal run of same-[`WordClass`] characters,
    /// skipping over runs of whitespace rather than stopping on them (the same "scan only the
    /// current line, not the whole buffer" perf discipline [`Self::previous_boundary`] already
    /// established - see that method's own docs for the measured cost of a whole-buffer scan on
    /// a large file). Crossing a line boundary (`offset` already at/before this line's own
    /// start) lands on the previous line's real end, exactly like [`Self::previous_boundary`].
    ///
    /// Deliberately *not* `unicode_segmentation::UnicodeSegmentation::split_word_bound_indices`
    /// (this buffer's own grapheme-boundary methods' crate, and this method's first real
    /// implementation): that crate's word boundaries are UAX #29's, designed for natural-language
    /// prose, where e.g. `WB6`/`WB7` deliberately keep `foo.bar` as *one* unbroken word (a
    /// mid-word `.`/`'`/`:` between two letters doesn't break, matching "don't"/"e.g." staying
    /// whole) - real, correct behavior for prose, but wrong for source code, where every real
    /// code editor's own word-navigation stops at `.` in `foo.bar()` (confirmed by writing the
    /// real test this docstring sits above against that assumption first - it failed against the
    /// UAX #29 result, `foo.bar` treated as one hop, not the real, expected two).
    fn previous_word_boundary(&self, offset: usize) -> usize {
        let (line, _) = self.line_col_for_offset(offset);
        let Some(line_range) = self.line_ranges.get(line).cloned() else {
            return 0;
        };
        if offset <= line_range.start {
            return if line == 0 {
                0
            } else {
                self.line_ranges[line - 1].end
            };
        }
        let local_offset = offset - line_range.start;
        let text = &self.content[line_range.clone()];
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut cursor = chars.iter().rposition(|&(index, _)| index < local_offset);
        while let Some(pos) = cursor {
            if word_class(chars[pos].1) == WordClass::Whitespace {
                cursor = pos.checked_sub(1);
            } else {
                break;
            }
        }
        let Some(mut start) = cursor else {
            return line_range.start;
        };
        let class = word_class(chars[start].1);
        while start > 0 && word_class(chars[start - 1].1) == class {
            start -= 1;
        }
        line_range.start + chars[start].0
    }

    /// The mirror of [`Self::previous_word_boundary`] - see its own docs for why this is a
    /// hand-classified scan rather than `unicode_segmentation`'s UAX #29 word boundaries.
    fn next_word_boundary(&self, offset: usize) -> usize {
        let (line, _) = self.line_col_for_offset(offset);
        let Some(line_range) = self.line_ranges.get(line).cloned() else {
            return self.content.len();
        };
        if offset >= line_range.end {
            return self
                .line_ranges
                .get(line + 1)
                .map(|next| next.start)
                .unwrap_or(self.content.len());
        }
        let local_offset = offset - line_range.start;
        let text = &self.content[line_range.clone()];
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut cursor = chars.iter().position(|&(index, _)| index >= local_offset);
        while let Some(pos) = cursor {
            if word_class(chars[pos].1) == WordClass::Whitespace {
                cursor = (pos + 1 < chars.len()).then_some(pos + 1);
            } else {
                break;
            }
        }
        let Some(mut end) = cursor else {
            return line_range.end;
        };
        let class = word_class(chars[end].1);
        while end + 1 < chars.len() && word_class(chars[end + 1].1) == class {
            end += 1;
        }
        let end_byte = chars
            .get(end + 1)
            .map(|&(index, _)| index)
            .unwrap_or(text.len());
        line_range.start + end_byte
    }

    /// `Ctrl+Left`: collapses a real selection to its start, or moves the caret to the start of
    /// the previous real word.
    pub fn move_word_left(&mut self) {
        self.goal_column = None;
        if self.selected_range.is_empty() {
            let previous = self.previous_word_boundary(self.cursor_offset());
            self.move_to(previous);
        } else {
            self.move_to(self.selected_range.start);
        }
    }

    /// The mirror of [`Self::move_word_left`].
    pub fn move_word_right(&mut self) {
        self.goal_column = None;
        if self.selected_range.is_empty() {
            let next = self.next_word_boundary(self.cursor_offset());
            self.move_to(next);
        } else {
            self.move_to(self.selected_range.end);
        }
    }

    /// `Ctrl+Shift+Left`: extends the selection to the start of the previous real word.
    pub fn select_word_left(&mut self) {
        self.goal_column = None;
        let previous = self.previous_word_boundary(self.cursor_offset());
        self.select_to(previous);
    }

    /// `Ctrl+Shift+Right`: extends the selection to the end of the next real word.
    pub fn select_word_right(&mut self) {
        self.goal_column = None;
        let next = self.next_word_boundary(self.cursor_offset());
        self.select_to(next);
    }

    /// Double-click word select (GitHub issue #27): selects the maximal real same-[`WordClass`]
    /// run touching `offset` (see [`Self::previous_word_boundary`]'s own docs for why this is a
    /// hand-classified scan, not `unicode_segmentation`'s natural-language-oriented UAX #29 word
    /// boundaries). `offset` landing on whitespace (or an empty line) selects nothing - a plain
    /// caret at `offset` rather than fabricating a plausible-looking word that isn't really
    /// there.
    pub fn select_word_at(&mut self, offset: usize) {
        self.goal_column = None;
        let (line, _) = self.line_col_for_offset(offset);
        let Some(line_range) = self.line_ranges.get(line).cloned() else {
            self.move_to(offset);
            return;
        };
        let local_offset = offset
            .saturating_sub(line_range.start)
            .min(line_range.end - line_range.start);
        let text = &self.content[line_range.clone()];
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        // The char the click actually landed on/just before - the last char whose byte range
        // contains `local_offset`, or (a click past the last char, e.g. right at the line's real
        // end) the last char on the line.
        let Some(mut pos) = chars
            .iter()
            .position(|&(index, ch)| local_offset < index + ch.len_utf8())
            .or_else(|| chars.len().checked_sub(1))
        else {
            self.move_to(offset);
            return;
        };
        let class = word_class(chars[pos].1);
        if class == WordClass::Whitespace {
            self.move_to(offset);
            return;
        }
        let mut start = pos;
        while start > 0 && word_class(chars[start - 1].1) == class {
            start -= 1;
        }
        while pos + 1 < chars.len() && word_class(chars[pos + 1].1) == class {
            pos += 1;
        }
        let end_byte = chars
            .get(pos + 1)
            .map(|&(index, _)| index)
            .unwrap_or(text.len());
        self.selected_range = line_range.start + chars[start].0..line_range.start + end_byte;
        self.selection_reversed = false;
    }

    /// Triple-click line select (GitHub issue #27): selects `line`'s whole real text, excluding
    /// its own line-ending bytes - a no-op (no panic, no change) for an out-of-range `line`.
    pub fn select_line_at(&mut self, line: usize) {
        self.goal_column = None;
        let Some(range) = self.line_ranges.get(line).cloned() else {
            return;
        };
        self.selected_range = range;
        self.selection_reversed = false;
    }

    /// `Home`: moves the caret to the start of its current line, at every real cursor at once.
    pub fn move_home(&mut self) {
        self.move_every_cursor(Self::move_home_one_cursor);
    }

    fn move_home_one_cursor(&mut self) {
        self.goal_column = None;
        let (line, _) = self.line_col_for_offset(self.cursor_offset());
        self.move_to(self.offset_for_line_col(line, 0));
    }

    /// `End`: moves the caret to the end of its current line's real text (before any line-ending
    /// bytes), at every real cursor at once.
    pub fn move_end(&mut self) {
        self.move_every_cursor(Self::move_end_one_cursor);
    }

    fn move_end_one_cursor(&mut self) {
        self.goal_column = None;
        let (line, _) = self.line_col_for_offset(self.cursor_offset());
        let len = self.line_len(line);
        self.move_to(self.offset_for_line_col(line, len));
    }

    /// The real target offset for a vertical move: the current line's own remembered
    /// [`Self::goal_column`] (set from the *current* column the first time this is called after
    /// a non-vertical move), applied to the line `delta` rows away, clamped to that line's real
    /// length - a documented byte-column approximation, not true visual-width-aware, matching
    /// this app's monospace-only rendering (see [`Self::goal_column`]'s own docs).
    fn vertical_offset(&mut self, delta: i64) -> usize {
        let (line, col) = self.line_col_for_offset(self.cursor_offset());
        let goal = *self.goal_column.get_or_insert(col);
        let target_line = if delta < 0 {
            line.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            let last = self.line_ranges.len().saturating_sub(1);
            (line + delta as usize).min(last)
        };
        self.offset_for_line_col(target_line, goal)
    }

    /// `Up`: moves the caret to the previous line, at the real remembered goal column, at every
    /// real cursor at once.
    pub fn move_up(&mut self) {
        self.move_every_cursor(Self::move_up_one_cursor);
    }

    fn move_up_one_cursor(&mut self) {
        let target = self.vertical_offset(-1);
        let goal = self.goal_column;
        self.move_to(target);
        self.goal_column = goal;
    }

    /// `Down`: the mirror of [`Self::move_up`].
    pub fn move_down(&mut self) {
        self.move_every_cursor(Self::move_down_one_cursor);
    }

    fn move_down_one_cursor(&mut self) {
        let target = self.vertical_offset(1);
        let goal = self.goal_column;
        self.move_to(target);
        self.goal_column = goal;
    }

    /// `Shift+Up`: extends the selection to the previous line at the real remembered goal column,
    /// at every real cursor at once.
    pub fn select_up(&mut self) {
        self.move_every_cursor(Self::select_up_one_cursor);
    }

    fn select_up_one_cursor(&mut self) {
        let target = self.vertical_offset(-1);
        let goal = self.goal_column;
        self.select_to(target);
        self.goal_column = goal;
    }

    /// `Shift+Down`: the mirror of [`Self::select_up`].
    pub fn select_down(&mut self) {
        self.move_every_cursor(Self::select_down_one_cursor);
    }

    fn select_down_one_cursor(&mut self) {
        let target = self.vertical_offset(1);
        let goal = self.goal_column;
        self.select_to(target);
        self.goal_column = goal;
    }

    /// Applies a pure cursor-movement method (no text change - `Self::move_left_one_cursor`/
    /// `Self::select_up_one_cursor`/etc.) to every real cursor at once, not just the primary - see
    /// this module's own "Multi-cursor" docs for why an arrow key must move every active cursor
    /// together, not silently strand every secondary wherever it was left. A no-op wrapper (calls
    /// `mover` once, directly) whenever [`Self::secondary_cursors`] is empty, so ordinary
    /// single-cursor use pays no extra cost and behaves exactly as before this method existed.
    ///
    /// Each secondary cursor's own [`Self::goal_column`] is deliberately *not* carried across
    /// consecutive vertical-move presses the way the primary's already is (reset to `None` before
    /// each secondary's own `mover` call below) - a documented, honest scope cut: giving every
    /// cursor its own independent sticky column would need a per-cursor goal-column field, real
    /// design work this phase's priority (a correct core multi-cursor model, occurrence search,
    /// and simultaneous edits) didn't reach. In practice this only matters for a *second*
    /// consecutive `Up`/`Down` press after crossing a ragged-length line while multiple cursors are
    /// active - a real, narrow, cosmetic gap, not a correctness bug (every cursor still lands on a
    /// real, valid position on every press).
    fn move_every_cursor<F>(&mut self, mut mover: F)
    where
        F: FnMut(&mut Self),
    {
        if self.secondary_cursors.is_empty() {
            mover(self);
            return;
        }
        // Taken (not merely cloned) *before* the primary's own `mover` call below - `Self::
        // move_to`/`Self::select_to` (which every `mover` implementation bottoms out in) clear
        // `Self::secondary_cursors` as their own first step (see their own docs), so this must
        // already be empty before that first call, or that clear would silently discard every
        // secondary before this method ever gets to move them.
        let saved_secondaries = std::mem::take(&mut self.secondary_cursors);
        mover(self);
        let moved_primary = (self.selected_range.clone(), self.selection_reversed);
        let primary_goal_column = self.goal_column;

        let mut new_secondaries = Vec::with_capacity(saved_secondaries.len());
        for cursor in saved_secondaries {
            self.selected_range = cursor.range;
            self.selection_reversed = cursor.reversed;
            self.goal_column = None;
            mover(self);
            new_secondaries.push(SecondaryCursor {
                range: self.selected_range.clone(),
                reversed: self.selection_reversed,
            });
        }

        self.selected_range = moved_primary.0;
        self.selection_reversed = moved_primary.1;
        self.goal_column = primary_goal_column;
        self.secondary_cursors = new_secondaries;
        self.merge_colliding_cursors();
    }

    /// Real 0-indexed `(line, column)` for a byte `offset` into [`Self::content`] - both in bytes,
    /// derived from [`Self::line_ranges`] via binary search (real `O(log n)`, not a linear scan
    /// over every line, so this stays cheap on a large file even though it's called on every
    /// cursor-moving action). An `offset` that falls inside a line's own line-ending bytes (rare -
    /// only reachable from an externally-supplied range, never from this type's own cursor math)
    /// clamps to that line's real end.
    pub fn line_col_for_offset(&self, offset: usize) -> (usize, usize) {
        if self.line_ranges.is_empty() {
            return (0, 0);
        }
        let offset = offset.min(self.content.len());
        let line = match self
            .line_ranges
            .binary_search_by(|range| range.start.cmp(&offset))
        {
            Ok(index) => index,
            Err(insert_index) => insert_index.saturating_sub(1),
        };
        let range = &self.line_ranges[line];
        let col = offset
            .saturating_sub(range.start)
            .min(range.end - range.start);
        (line, col)
    }

    /// The real inverse of [`Self::line_col_for_offset`]: the byte offset for `(line, col)`,
    /// clamping `line` into range and `col` to that line's real length, then snapping to the
    /// nearest real char boundary (a byte-column approximation can otherwise land mid-character).
    pub fn offset_for_line_col(&self, line: usize, col: usize) -> usize {
        if self.line_ranges.is_empty() {
            return 0;
        }
        let line = line.min(self.line_ranges.len() - 1);
        let range = &self.line_ranges[line];
        let len = range.end - range.start;
        self.floor_char_boundary(range.start + col.min(len))
    }

    /// `line`'s real text length in bytes (excluding line-ending bytes) - `0` for an out-of-range
    /// `line` rather than panicking.
    pub fn line_len(&self, line: usize) -> usize {
        self.line_ranges
            .get(line)
            .map(|range| range.end - range.start)
            .unwrap_or(0)
    }

    /// The real selection clipped to `line`'s own bytes, translated to a range local to that
    /// line's text (`0..line_len`) - `None` when the selection is empty or doesn't touch `line`
    /// at all. `crate::code_surface::editing`'s per-row painter reads this for every visible row the
    /// selection intersects, not just the caret's own row.
    pub fn selection_within_line(&self, line: usize) -> Option<Range<usize>> {
        if self.selected_range.is_empty() {
            return None;
        }
        let range = self.line_ranges.get(line)?;
        let start = self.selected_range.start.max(range.start);
        let end = self.selected_range.end.min(range.end);
        (start < end).then(|| start - range.start..end - range.start)
    }

    /// The real caret position local to `line`'s text - `None` when the caret isn't on `line` at
    /// all. Returns `Some` even while there's an active selection (GitHub issue #27: "caret is
    /// visible against every theme background, including in selected regions") - this used to
    /// return `None` whenever [`Self::selected_range`] was non-empty at all, matching
    /// `vendor/zed/crates/gpui/examples/input.rs`'s own single-line `TextElement`, which never
    /// draws a separate caret glyph over its own selection fill. That's a reasonable choice for
    /// a one-line input, but a real code editor's own caret is real, useful information
    /// (`cursor_offset`, this buffer's own "active end of the selection") a user still needs to
    /// see while a selection is active - not fabricated, since [`Self::cursor_offset`] already
    /// names an exact, real position regardless of whether [`Self::selected_range`] is empty.
    pub fn cursor_within_line(&self, line: usize) -> Option<usize> {
        let range = self.line_ranges.get(line)?;
        let offset = self.cursor_offset();
        // `then_some` eagerly evaluates its argument even when the condition is false - using it
        // here (as this code originally did) computed `offset - range.start` unconditionally and
        // panicked with a real `usize` underflow the very first time a caret's line didn't match
        // `line` (i.e. on every row except the caret's own). `then(|| ...)` is lazy, so the
        // subtraction only ever runs once the bounds check has already passed.
        (offset >= range.start && offset <= range.end).then(|| offset - range.start)
    }

    /// The real IME composition range clipped to `line`, local to its text - `None` when there's
    /// no active composition or it doesn't touch `line`.
    pub fn marked_within_line(&self, line: usize) -> Option<Range<usize>> {
        let marked = self.marked_range.as_ref()?;
        let range = self.line_ranges.get(line)?;
        let start = marked.start.max(range.start);
        let end = marked.end.min(range.end);
        (start <= end).then(|| start - range.start..end - range.start)
    }

    // ---- Multi-cursor (Revision R13, issue #28) - see this module's own top "Multi-cursor" docs
    // for the overall design this section implements. ----

    /// `true` while more than one real cursor is active.
    pub fn has_multiple_cursors(&self) -> bool {
        !self.secondary_cursors.is_empty()
    }

    /// The total number of real cursors - always `>= 1` (the primary always counts).
    pub fn cursor_count(&self) -> usize {
        1 + self.secondary_cursors.len()
    }

    /// Every real cursor's own selection range - the primary first, then
    /// [`Self::secondary_cursors`] in storage order (not guaranteed sorted by position). Used by
    /// occurrence search (to know what's already covered) and by tests that need to enumerate
    /// every real cursor.
    pub fn all_selection_ranges(&self) -> Vec<Range<usize>> {
        let mut ranges = Vec::with_capacity(self.cursor_count());
        ranges.push(self.selected_range.clone());
        ranges.extend(
            self.secondary_cursors
                .iter()
                .map(|cursor| cursor.range.clone()),
        );
        ranges
    }

    /// The real selection ranges of every *secondary* cursor (never the primary - see
    /// [`Self::selection_within_line`] for that) clipped to `line`'s own bytes and translated
    /// local to that line's text - one entry per secondary cursor with a non-empty selection
    /// touching `line`. `crate::code_surface::editing`'s per-row painter reads this to draw every
    /// real secondary selection fill, exactly like [`Self::selection_within_line`] already does
    /// for the primary.
    pub fn secondary_selections_within_line(&self, line: usize) -> Vec<Range<usize>> {
        let Some(range) = self.line_ranges.get(line) else {
            return Vec::new();
        };
        self.secondary_cursors
            .iter()
            .filter(|cursor| !cursor.range.is_empty())
            .filter_map(|cursor| {
                let start = cursor.range.start.max(range.start);
                let end = cursor.range.end.min(range.end);
                (start < end).then(|| start - range.start..end - range.start)
            })
            .collect()
    }

    /// The real caret position(s) of every *secondary* cursor local to `line`'s text - the
    /// multi-cursor mirror of [`Self::cursor_within_line`]. Only secondary cursors with an
    /// *empty* selection contribute a caret bar, matching that method's own "no caret while a
    /// real selection is active" rule, applied per-cursor.
    pub fn secondary_cursors_within_line(&self, line: usize) -> Vec<usize> {
        let Some(range) = self.line_ranges.get(line) else {
            return Vec::new();
        };
        self.secondary_cursors
            .iter()
            .filter(|cursor| cursor.range.is_empty())
            .filter_map(|cursor| {
                let offset = cursor.range.start;
                (offset >= range.start && offset <= range.end).then(|| offset - range.start)
            })
            .collect()
    }

    /// `Esc`: drops every secondary cursor, leaving just the primary - the real "back to a single
    /// cursor" escape hatch every multi-cursor editor provides. Returns `false` (a genuine no-op)
    /// when there was only ever one cursor, so `crate::code_surface::editing`'s own handler can
    /// let an unrelated, real single-cursor `Escape` meaning (dismissing some other overlay) take
    /// over instead of always claiming the keystroke.
    pub fn collapse_to_single_cursor(&mut self) -> bool {
        if self.secondary_cursors.is_empty() {
            return false;
        }
        self.secondary_cursors.clear();
        true
    }

    /// Alt+click: adds a brand-new, empty cursor at `offset`, keeping every existing cursor - the
    /// real arbitrary-position complement to [`Self::add_next_occurrence_cursor`]'s text-search-
    /// driven placement. The new cursor becomes primary (the same "newest cursor is primary"
    /// convention [`Self::add_next_occurrence_cursor`] itself uses, so a further `Ctrl+D` always
    /// searches forward from wherever the user most recently pointed).
    pub fn add_cursor_at(&mut self, offset: usize) {
        let offset = self.floor_char_boundary(offset.min(self.content.len()));
        let previous_primary = SecondaryCursor {
            range: self.selected_range.clone(),
            reversed: self.selection_reversed,
        };
        self.secondary_cursors.push(previous_primary);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.goal_column = None;
        self.merge_colliding_cursors();
    }

    /// Real, Unicode-aware "is this a word character" classification - alphanumeric or `_`,
    /// matching VS Code's own default `wordSeparators` treatment (ASCII punctuation/whitespace all
    /// separate words; letters/digits/underscore from any script do not). Shared by
    /// [`Self::word_range_at`]'s own forward/backward scans.
    fn is_word_char(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    fn word_scan_start(&self, offset: usize) -> usize {
        let mut start = offset;
        for (index, ch) in self.content[..offset].char_indices().rev() {
            if !Self::is_word_char(ch) {
                break;
            }
            start = index;
        }
        start
    }

    fn word_scan_end(&self, offset: usize) -> usize {
        let mut end = offset;
        for (index, ch) in self.content[offset..].char_indices() {
            if !Self::is_word_char(ch) {
                break;
            }
            end = offset + index + ch.len_utf8();
        }
        end
    }

    /// The real word-boundary range touching byte offset `offset` from either side - matches
    /// every real editor's own "word under the caret" convention: a caret with no selection
    /// exactly between two words (or inside a run of whitespace/punctuation with no adjacent word
    /// character at all) touches neither, and this returns `None`. Classification is
    /// [`Self::is_word_char`]'s own Unicode-aware rule, matching VS Code's default.
    pub fn word_range_at(&self, offset: usize) -> Option<Range<usize>> {
        let offset = self.floor_char_boundary(offset.min(self.content.len()));
        let start = self.word_scan_start(offset);
        let end = self.word_scan_end(offset);
        (start < end).then_some(start..end)
    }

    /// The real search-text source for occurrence matching - the primary cursor's own currently
    /// selected text. Occurrence search always reads from the primary, never a secondary: every
    /// method that adds a cursor keeps the newest occurrence as [`Self::selected_range`], pushing
    /// whatever was previously primary into [`Self::secondary_cursors`] instead - see
    /// [`Self::add_next_occurrence_cursor`]'s own docs for why that convention makes "the
    /// primary's own text" always the real, intended search source.
    fn occurrence_search_text(&self) -> Option<String> {
        if self.selected_range.is_empty() {
            return None;
        }
        self.content
            .get(self.selected_range.clone())
            .map(|text| text.to_string())
    }

    /// Real forward, wrapping, case-sensitive, plain-substring search for the next occurrence of
    /// `needle` starting at or after `from`, skipping any byte range an existing real cursor
    /// (primary or secondary) already covers - shared by [`Self::add_next_occurrence_cursor`]/
    /// [`Self::skip_current_occurrence`]. Wraps around to the start of the buffer once the search
    /// reaches the end without finding a not-already-selected match, matching VS Code's own "Add
    /// Selection To Next Find Match" wraparound. Returns `None` once no further real occurrence
    /// exists anywhere in the buffer.
    fn find_next_occurrence(&self, needle: &str, from: usize) -> Option<Range<usize>> {
        if needle.is_empty() {
            return None;
        }
        let existing = self.all_selection_ranges();
        let is_already_selected =
            |range: &Range<usize>| existing.iter().any(|selected| selected == range);
        let search_from = |start: usize| -> Option<Range<usize>> {
            self.content
                .get(start..)?
                .match_indices(needle)
                .map(|(index, matched)| (start + index)..(start + index + matched.len()))
                .find(|range| !is_already_selected(range))
        };
        search_from(from.min(self.content.len())).or_else(|| search_from(0))
    }

    /// `Ctrl+D` ("Add Selection To Next Find Match"): with no active selection at all, selects the
    /// real word under the caret ([`Self::word_range_at`]) - VS Code's own first step. With any
    /// active selection (freshly word-selected just now, or a pre-existing manual selection),
    /// finds the next real occurrence of that selection's own exact text and adds it as a brand-
    /// new cursor - VS Code's own second step. Both described as one user-facing shortcut here,
    /// same as VS Code's own single `Ctrl+D` binding. Returns `false` (a genuine no-op) when
    /// there's no word under an empty caret, or when every real occurrence of the search text is
    /// already covered by an existing cursor.
    pub fn select_word_or_add_next_occurrence(&mut self) -> bool {
        if self.selected_range.is_empty() && self.secondary_cursors.is_empty() {
            let Some(word) = self.word_range_at(self.cursor_offset()) else {
                return false;
            };
            self.selected_range = word;
            self.selection_reversed = false;
            return true;
        }
        self.add_next_occurrence_cursor()
    }

    /// Adds a brand-new cursor at the next real occurrence of the primary selection's own text,
    /// keeping every existing cursor - see [`Self::select_word_or_add_next_occurrence`]'s own
    /// docs. The new occurrence becomes primary; the previous primary is pushed into
    /// [`Self::secondary_cursors`] - so a further `Ctrl+D` always searches forward from the
    /// rightmost cursor's own end, walking through the buffer one real match at a time.
    fn add_next_occurrence_cursor(&mut self) -> bool {
        let Some(needle) = self.occurrence_search_text() else {
            return false;
        };
        let search_from = self
            .all_selection_ranges()
            .iter()
            .map(|range| range.end)
            .max()
            .unwrap_or(self.selected_range.end);
        let Some(next) = self.find_next_occurrence(&needle, search_from) else {
            return false;
        };
        self.secondary_cursors.push(SecondaryCursor {
            range: self.selected_range.clone(),
            reversed: self.selection_reversed,
        });
        self.selected_range = next;
        self.selection_reversed = false;
        self.goal_column = None;
        self.merge_colliding_cursors();
        true
    }

    /// `Ctrl+K Ctrl+D` ("Move Last Selection To Next Find Match"): replaces the primary cursor
    /// with the next real occurrence of its own selected text, without keeping the current
    /// occurrence selected - the real "skip this one, keep looking" complement to
    /// [`Self::add_next_occurrence_cursor`], which always keeps every previous occurrence. Every
    /// other existing cursor is left untouched, matching VS Code's own real behavior (it only ever
    /// acts on the most recently added selection). Returns `false` when there's no primary
    /// selection to search from, or no further real occurrence exists.
    pub fn skip_current_occurrence(&mut self) -> bool {
        let Some(needle) = self.occurrence_search_text() else {
            return false;
        };
        let Some(next) = self.find_next_occurrence(&needle, self.selected_range.end) else {
            return false;
        };
        self.selected_range = next;
        self.selection_reversed = false;
        self.goal_column = None;
        self.merge_colliding_cursors();
        true
    }

    /// `Ctrl+Shift+L` ("Select All Occurrences of Find Match"): selects the real word under the
    /// caret first if there's no active selection yet (the same first step
    /// [`Self::select_word_or_add_next_occurrence`] takes), then replaces every existing cursor
    /// with one cursor per real occurrence of that text anywhere in the buffer. Not additive on
    /// top of whatever cursors already existed - matches VS Code's own "select all" semantics:
    /// this always yields exactly one cursor per real occurrence, never a mix of old and new.
    /// Returns `false` when there's no word/selection to search for.
    pub fn select_all_occurrences(&mut self) -> bool {
        if self.selected_range.is_empty() && self.secondary_cursors.is_empty() {
            let Some(word) = self.word_range_at(self.cursor_offset()) else {
                return false;
            };
            self.selected_range = word;
            self.selection_reversed = false;
        }
        let Some(needle) = self.occurrence_search_text() else {
            return false;
        };
        let occurrences: Vec<Range<usize>> = self
            .content
            .match_indices(needle.as_str())
            .map(|(index, matched)| index..(index + matched.len()))
            .collect();
        let Some(first) = occurrences.first().cloned() else {
            return false;
        };
        self.secondary_cursors = occurrences
            .into_iter()
            .skip(1)
            .map(|range| SecondaryCursor {
                range,
                reversed: false,
            })
            .collect();
        self.selected_range = first;
        self.selection_reversed = false;
        self.goal_column = None;
        true
    }

    /// Merges any real cursors (primary or secondary) that now overlap, or sit at the exact same
    /// empty-caret position, into one - VS Code's own "cursors merge when they collide" behavior.
    /// Called at the end of every real operation that could produce a collision: adding a cursor,
    /// moving every cursor together, or applying a multi-cursor edit. A no-op (returns
    /// immediately) whenever there's only the primary - the overwhelmingly common case, so this
    /// never pays real sorting/scanning cost in ordinary single-cursor use.
    fn merge_colliding_cursors(&mut self) {
        if self.secondary_cursors.is_empty() {
            return;
        }

        struct Tagged {
            range: Range<usize>,
            reversed: bool,
            is_primary: bool,
        }

        let mut all: Vec<Tagged> = Vec::with_capacity(self.cursor_count());
        all.push(Tagged {
            range: self.selected_range.clone(),
            reversed: self.selection_reversed,
            is_primary: true,
        });
        for cursor in &self.secondary_cursors {
            all.push(Tagged {
                range: cursor.range.clone(),
                reversed: cursor.reversed,
                is_primary: false,
            });
        }
        all.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then(a.range.end.cmp(&b.range.end))
        });

        let mut merged: Vec<Tagged> = Vec::with_capacity(all.len());
        for item in all {
            // Collides with the last accepted cursor when the two real ranges genuinely overlap,
            // or both are empty carets sitting at the exact same offset (two non-overlapping,
            // merely *adjacent* real selections/carets - e.g. `0..3` next to `3..6` - are
            // deliberately left as two distinct cursors: "touching" is not "colliding").
            let collides = merged.last().is_some_and(|last: &Tagged| {
                item.range.start < last.range.end
                    || (item.range.start == last.range.end
                        && item.range.is_empty()
                        && last.range.is_empty())
            });
            if collides {
                if let Some(last) = merged.last_mut() {
                    last.range.end = last.range.end.max(item.range.end);
                    last.is_primary = last.is_primary || item.is_primary;
                }
            } else {
                merged.push(item);
            }
        }

        let primary_index = merged.iter().position(|item| item.is_primary).unwrap_or(0);
        let primary = merged.remove(primary_index);
        self.selected_range = primary.range;
        self.selection_reversed = primary.reversed;
        self.secondary_cursors = merged
            .into_iter()
            .map(|item| SecondaryCursor {
                range: item.range,
                reversed: item.reversed,
            })
            .collect();
    }

    /// Applies one real edit per cursor - primary and every secondary - as a single atomic
    /// multi-cursor operation: `compute` is handed `self` (read-only) and that cursor's own
    /// current range, and returns the real byte range to splice plus the replacement text for it.
    /// An `(range, text)` where `range` is empty and `text` is empty is treated as a genuine
    /// per-cursor no-op (e.g. Backspace at the very start of the buffer for one cursor while
    /// others still have real deletions to make) - skipped without a real splice, so it never
    /// spuriously marks the buffer's highlighting dirty for a cursor that didn't actually change
    /// anything.
    ///
    /// Processed from the rightmost cursor to the leftmost (by `range.start`), the standard multi-
    /// cursor discipline every real editor's own batch-edit application relies on: splicing the
    /// rightmost cursor's own edit first can never invalidate the still-unprocessed byte offsets of
    /// any cursor still to its left, so `compute`'s own per-cursor boundary lookups (e.g.
    /// `Self::previous_boundary` for a Backspace with no selection) never need any real cross-
    /// cursor offset bookkeeping of their own - only this method's own bookkeeping of each
    /// cursor's *new* caret position (recorded as it's produced, restored onto every real cursor
    /// only once every real splice has landed) does. Every real cursor collapses to an empty caret
    /// at its own post-edit position - the "typing/deleting closes any selection" behavior every
    /// one of this buffer's own single-cursor edit methods already has, applied per-cursor here.
    ///
    /// **Records every real per-cursor splice into [`Self::history`] as one group** (GitHub issue
    /// #17's own history, extended here for issue #28) - see this module's own "Multi-cursor edits
    /// are one real undo step" docs for the chaining technique and its one documented limitation
    /// (only the primary cursor's own caret/selection is restored by undo/redo, not every
    /// secondary). `kind` is shared by every edit in the batch on purpose: typing/pasting the same
    /// text at every cursor and deleting at every cursor are each internally uniform, and a shared
    /// kind is what lets [`TextHistory::record`]'s own `top.kind != kind` check never split one
    /// multi-cursor keystroke into more than one group.
    fn apply_at_every_cursor<F>(&mut self, kind: EditKind, mut compute: F)
    where
        F: FnMut(&Self, Range<usize>) -> (Range<usize>, String),
    {
        // Defensive: every real caller keeps `Self::secondary_cursors` merge-clean between
        // keystrokes already (every mutating multi-cursor operation ends with
        // `Self::merge_colliding_cursors`), but this method's own right-to-left bookkeeping below
        // is only correct for non-overlapping input ranges - two identical/overlapping cursors
        // reaching this method would otherwise each independently compute a real edit against
        // content the *other* has already spliced, corrupting one of them. A cheap, real safety
        // net rather than relying purely on caller discipline.
        self.merge_colliding_cursors();

        let real_before = self.selection_snapshot();

        let mut ordered: Vec<(usize, Range<usize>)> = Vec::with_capacity(self.cursor_count());
        ordered.push((0, self.selected_range.clone()));
        for (index, cursor) in self.secondary_cursors.iter().enumerate() {
            ordered.push((index + 1, cursor.range.clone()));
        }
        ordered.sort_by(|a, b| b.1.start.cmp(&a.1.start).then(b.1.end.cmp(&a.1.end)));
        let last_index = ordered.len().saturating_sub(1);

        let now = Instant::now();
        let mut chain_before = real_before;
        let mut new_positions: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::with_capacity(ordered.len());
        for (loop_index, (id, range)) in ordered.into_iter().enumerate() {
            let (edit_range, text) = compute(self, range);
            let edit_range = self.clamp_range(edit_range);
            if edit_range.is_empty() && text.is_empty() {
                new_positions.insert(id, edit_range.start);
                continue;
            }
            let removed = self
                .content
                .get(edit_range.clone())
                .map(|removed| removed.to_string())
                .unwrap_or_default();
            let delta = text.len() as isize - (edit_range.end - edit_range.start) as isize;
            self.splice_lines(edit_range.clone(), &text);
            // Every position already recorded belongs to a cursor processed earlier in this
            // right-to-left pass, i.e. one that sat to the right of (or exactly at) this edit's
            // own start - this edit just moved every one of those real positions by its own byte
            // delta, so each must be shifted to stay correct once every real splice has landed.
            for pos in new_positions.values_mut() {
                if *pos >= edit_range.start {
                    *pos = (*pos as isize + delta) as usize;
                }
            }
            let new_pos = edit_range.start + text.len();
            new_positions.insert(id, new_pos);

            if !self.replaying && removed != text {
                // The group's real, stored `after` (what redo restores) is the *primary* cursor's
                // own final position - see this method's own docs on why only the primary's
                // selection is restorable at all. Every other edit in the chain gets a synthetic
                // caret that only needs to satisfy `TextHistory`'s own before-equals-previous-after
                // continuity check so the whole batch coalesces into one group, never a value a
                // caller reads back out on its own.
                let after = if loop_index == last_index {
                    let primary_pos = new_positions.get(&0).copied().unwrap_or(new_pos);
                    SelectionSnapshot::caret(primary_pos)
                } else {
                    SelectionSnapshot::caret(new_pos)
                };
                self.history.record(
                    TextEdit {
                        at: edit_range.start,
                        removed,
                        inserted: text,
                    },
                    chain_before,
                    after,
                    kind,
                    now,
                );
                chain_before = after;
            }
        }

        if let Some(&pos) = new_positions.get(&0) {
            self.selected_range = pos..pos;
            self.selection_reversed = false;
        }
        for (index, cursor) in self.secondary_cursors.iter_mut().enumerate() {
            if let Some(&pos) = new_positions.get(&(index + 1)) {
                cursor.range = pos..pos;
                cursor.reversed = false;
            }
        }
        self.marked_range = None;
        self.goal_column = None;
        self.merge_colliding_cursors();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn buffer(content: &str) -> EditBuffer {
        EditBuffer::new(
            PathBuf::from("/tmp/test.rs"),
            content.to_string(),
            Some("rs".to_string()),
            None,
            content.len() as u64,
        )
    }

    #[test]
    fn insert_at_position_splices_real_text_and_moves_the_caret_after_it() {
        let mut buf = buffer("fn main() {}\n");
        buf.move_to(3);
        buf.replace_range(None, "run_");
        assert_eq!(buf.content, "fn run_main() {}\n");
        assert_eq!(buf.selected_range, 7..7);
        assert!(buf.is_dirty());
    }

    #[test]
    fn delete_range_removes_the_real_selected_text() {
        let mut buf = buffer("fn main() {}\n");
        buf.selected_range = 3..7; // "main"
        buf.replace_range(None, "");
        assert_eq!(buf.content, "fn () {}\n");
        assert_eq!(buf.selected_range, 3..3);
    }

    #[test]
    fn multi_line_insert_splits_into_real_new_lines() {
        let mut buf = buffer("ab");
        buf.move_to(1);
        buf.replace_range(None, "\nX\n");
        assert_eq!(buf.content, "a\nX\nb");
        assert_eq!(buf.lines.len(), 3);
        assert_eq!(buf.lines[0].text, "a");
        assert_eq!(buf.lines[1].text, "X");
        assert_eq!(buf.lines[2].text, "b");
    }

    #[test]
    fn line_col_for_offset_at_the_very_start_of_an_empty_file_is_zero_zero() {
        let buf = buffer("");
        assert_eq!(buf.line_col_for_offset(0), (0, 0));
    }

    #[test]
    fn line_col_for_offset_at_the_very_end_of_the_file() {
        let buf = buffer("abc\ndef");
        assert_eq!(buf.line_col_for_offset(7), (1, 3));
    }

    #[test]
    fn line_col_for_offset_exactly_on_a_line_boundary_is_the_start_of_the_next_line() {
        let buf = buffer("abc\ndef");
        // Byte 4 is 'd', the first byte of line 1.
        assert_eq!(buf.line_col_for_offset(4), (1, 0));
    }

    #[test]
    fn offset_for_line_col_round_trips_with_line_col_for_offset() {
        let buf = buffer("hello\nworld\n!");
        for offset in [0usize, 3, 5, 6, 9, 11, 12, 13] {
            let (line, col) = buf.line_col_for_offset(offset);
            assert_eq!(
                buf.offset_for_line_col(line, col),
                offset,
                "offset {offset}"
            );
        }
    }

    #[test]
    fn grapheme_boundaries_do_not_split_a_real_multi_byte_emoji() {
        let mut buf = buffer("a\u{1f600}b"); // a, grinning-face emoji, b
        let emoji_start = "a".len();
        let emoji_end = emoji_start + "\u{1f600}".len();
        buf.move_to(emoji_end); // caret right after the emoji
        buf.move_left();
        assert_eq!(
            buf.selected_range,
            emoji_start..emoji_start,
            "left from right after the emoji must land before it whole, not mid-byte"
        );
        buf.move_right();
        assert_eq!(buf.selected_range, emoji_end..emoji_end);
    }

    #[test]
    fn grapheme_boundaries_do_not_split_a_real_combining_accent() {
        // "e" + combining acute accent (U+0301) - one real grapheme cluster, two chars.
        let mut buf = buffer("e\u{301}x");
        let cluster_end = "e\u{301}".len();
        buf.move_to(0);
        buf.move_right();
        assert_eq!(
            buf.selected_range,
            cluster_end..cluster_end,
            "one right-arrow press must skip the whole accented cluster, not stop mid-character"
        );
    }

    #[test]
    fn utf16_round_trip_covers_a_real_surrogate_pair_character() {
        // U+1F600 encodes as a UTF-16 surrogate pair (2 code units) and 4 UTF-8 bytes.
        let buf = buffer("a\u{1f600}b");
        let byte_after_emoji = "a\u{1f600}".len();
        let utf16_after_emoji = buf.offset_to_utf16(byte_after_emoji);
        assert_eq!(utf16_after_emoji, 1 + 2, "'a' (1) + surrogate pair (2)");
        assert_eq!(buf.offset_from_utf16(utf16_after_emoji), byte_after_emoji);
    }

    #[test]
    fn offset_for_position_resolves_a_real_lsp_line_and_character_to_a_byte_offset() {
        let buf = buffer("hello\nworld\n!");
        assert_eq!(buf.offset_for_position(0, 0), 0);
        assert_eq!(buf.offset_for_position(0, 3), 3);
        assert_eq!(buf.offset_for_position(1, 2), "hello\nwo".len());
    }

    #[test]
    fn offset_for_position_accounts_for_a_real_multi_byte_character_earlier_on_the_same_line() {
        // "café " - 'é' is 1 UTF-16 unit but 2 UTF-8 bytes, so the real byte offset of the
        // 'x' that follows must reflect that, not assume 1 UTF-16 unit == 1 byte.
        let buf = buffer("caf\u{e9} x");
        let x_utf16_character = 5; // c,a,f,é,space = 5 UTF-16 units before 'x'
        assert_eq!(
            buf.offset_for_position(0, x_utf16_character),
            "caf\u{e9} ".len()
        );
    }

    #[test]
    fn offset_for_position_clamps_a_real_out_of_range_line_rather_than_panicking() {
        let buf = buffer("abc\ndef");
        // Clamps to the real *last line* ("def"), character 0 - not the buffer's own end.
        assert_eq!(buf.offset_for_position(99, 0), "abc\n".len());
    }

    #[test]
    fn is_dirty_transitions_on_edit_and_clears_on_mark_saved() {
        let mut buf = buffer("abc");
        assert!(!buf.is_dirty());
        buf.move_to(3);
        buf.replace_range(None, "d");
        assert!(buf.is_dirty());
        buf.mark_saved(buf.content.clone(), None, buf.content.len() as u64);
        assert!(!buf.is_dirty());
    }

    /// The real race `Self::mark_saved` must not reintroduce: a save in flight for an older
    /// snapshot must not be applied against whatever `content` is by the time it completes - if
    /// the user typed more in the meantime, those newer keystrokes must still read as dirty.
    #[test]
    fn mark_saved_against_a_stale_snapshot_leaves_newer_edits_dirty() {
        let mut buf = buffer("abc");
        let written_snapshot = buf.content.clone(); // what a save would have captured
        buf.move_to(3);
        buf.replace_range(None, "d"); // a newer edit lands while that save is "in flight"
        assert_ne!(buf.content, written_snapshot);

        buf.mark_saved(written_snapshot, None, 3);
        assert!(
            buf.is_dirty(),
            "content moved past the snapshot that was actually written - it must still read \
             dirty, not be falsely marked saved"
        );
    }

    #[test]
    fn rehighlighting_after_an_edit_that_changes_syntax_meaning_changes_the_run_kind() {
        let mut buf = buffer("foo");
        // Immediately after the edit, `splice_lines` has run - text is correct, plain.
        buf.move_to(0);
        buf.replace_range(None, "fn ");
        assert_eq!(buf.content, "fn foo");
        assert!(buf.highlight_dirty);
        assert!(buf.lines[0]
            .runs
            .iter()
            .all(|(_, kind)| *kind == code_view::HighlightKind::Text));

        // A real re-highlight (as `AdeApp`'s debounced background task would apply) turns "fn"
        // into a real keyword and "foo" into a real function name.
        let spans = code_view::highlight_rust(&buf.content);
        let lines = code_view::build_lines(&buf.content, &spans);
        let content_snapshot = buf.content.clone();
        assert!(buf.apply_highlight(&content_snapshot, lines));
        assert!(!buf.highlight_dirty);
        let kinds: Vec<code_view::HighlightKind> =
            buf.lines[0].runs.iter().map(|(_, kind)| *kind).collect();
        assert!(kinds.contains(&code_view::HighlightKind::Keyword));
    }

    #[test]
    fn apply_highlight_is_rejected_when_content_has_moved_on_since_the_snapshot() {
        let mut buf = buffer("foo");
        let stale_snapshot = buf.content.clone();
        let spans = code_view::highlight_rust(&stale_snapshot);
        let stale_lines = code_view::build_lines(&stale_snapshot, &spans);

        buf.move_to(3);
        buf.replace_range(None, "bar");
        assert_ne!(buf.content, stale_snapshot);

        assert!(!buf.apply_highlight(&stale_snapshot, stale_lines));
        assert!(
            buf.highlight_dirty,
            "a stale highlight result must not clear the dirty flag"
        );
    }

    #[test]
    fn backspace_at_the_very_start_of_the_buffer_is_a_real_no_op() {
        let mut buf = buffer("abc");
        buf.move_to(0);
        buf.backspace();
        assert_eq!(buf.content, "abc");
    }

    #[test]
    fn delete_at_the_very_end_of_the_buffer_is_a_real_no_op() {
        let mut buf = buffer("abc");
        buf.move_to(3);
        buf.delete_forward();
        assert_eq!(buf.content, "abc");
    }

    #[test]
    fn arrow_right_crosses_a_real_line_boundary() {
        let mut buf = buffer("ab\ncd");
        buf.move_to(2); // right after 'b', before the newline
        buf.move_right();
        assert_eq!(
            buf.selected_range,
            3..3,
            "should land at the start of line 2 (\"c\")"
        );
    }

    #[test]
    fn arrow_left_crosses_a_real_line_boundary() {
        let mut buf = buffer("ab\ncd");
        buf.move_to(3); // start of line 2 ("c")
        buf.move_left();
        assert_eq!(
            buf.selected_range,
            2..2,
            "should land at the end of line 1 (\"ab\")"
        );
    }

    #[test]
    fn shift_right_selects_and_select_all_selects_the_whole_buffer() {
        let mut buf = buffer("hello");
        buf.move_to(0);
        buf.select_right();
        buf.select_right();
        assert_eq!(buf.selected_range, 0..2);
        buf.select_all();
        assert_eq!(buf.selected_range, 0..buf.content.len());
    }

    #[test]
    fn vertical_movement_preserves_a_real_goal_column_across_a_shorter_line() {
        let mut buf = buffer("hello\nhi\nworld");
        buf.move_to(4); // column 4 on line 0 ("hello")
        buf.move_down(); // line 1 ("hi") only has 2 columns - clamp
        assert_eq!(buf.line_col_for_offset(buf.cursor_offset()), (1, 2));
        buf.move_down(); // line 2 ("world") - goal column 4 should be restored, not stuck at 2
        assert_eq!(buf.line_col_for_offset(buf.cursor_offset()), (2, 4));
    }

    #[test]
    fn horizontal_movement_clears_the_goal_column() {
        let mut buf = buffer("hello\nhi\nworld");
        buf.move_to(4);
        buf.move_down();
        buf.move_left();
        buf.move_down();
        // After an explicit left-arrow, the goal column resets to wherever the caret now is,
        // not the original column 4.
        let (line, col) = buf.line_col_for_offset(buf.cursor_offset());
        assert_eq!(line, 2);
        assert_eq!(
            col, 1,
            "goal column should have reset to column 1 after move_left"
        );
    }

    #[test]
    fn home_and_end_move_to_the_real_line_boundaries() {
        let mut buf = buffer("hello\nworld");
        buf.move_to(8); // somewhere inside "world"
        buf.move_home();
        assert_eq!(buf.selected_range, 6..6);
        buf.move_end();
        assert_eq!(buf.selected_range, 11..11);
    }

    #[test]
    fn selection_within_line_clips_a_multi_line_selection_per_row() {
        let buf_content = "hello\nworld\n!";
        let mut buf = buffer(buf_content);
        buf.selected_range = 2..9; // "llo\nwor"
        assert_eq!(buf.selection_within_line(0), Some(2..5));
        assert_eq!(buf.selection_within_line(1), Some(0..3));
        assert_eq!(buf.selection_within_line(2), None);
    }

    /// The real regression this fix addresses: `cursor_within_line` used to compute
    /// `offset - range.start` *before* checking whether `offset >= range.start` (an eager
    /// `Option::then_some` argument, not the lazy `then(|| ...)` this now uses), panicking with a
    /// real `usize` subtract-with-overflow the moment it was asked about any line other than the
    /// caret's own - i.e. on every single row of a real multi-line file.
    #[test]
    fn cursor_within_line_does_not_panic_for_a_line_before_the_caret() {
        let mut buf = buffer("first\nsecond\nthird");
        buf.move_to(buf.content.len()); // caret on line 2 ("third")
        assert_eq!(buf.cursor_within_line(0), None);
        assert_eq!(buf.cursor_within_line(1), None);
        assert!(buf.cursor_within_line(2).is_some());
    }

    /// GitHub issue #27: "caret is visible against every theme background, including in
    /// selected regions" - `cursor_within_line` must keep reporting the real active end of the
    /// selection even while one is active, not go back to `None` the moment
    /// `selected_range` stops being empty.
    #[test]
    fn cursor_within_line_stays_some_while_a_selection_is_active() {
        let mut buf = buffer("hello world");
        buf.move_to(0);
        buf.select_to(5); // "hello" selected, caret (active end) at byte 5
        assert_eq!(
            buf.cursor_within_line(0),
            Some(5),
            "the real active end of the selection must still be reported as the caret position"
        );
    }

    #[test]
    fn replace_and_mark_range_records_a_real_marked_range_and_clears_on_empty_text() {
        let mut buf = buffer("ab");
        buf.move_to(1);
        buf.replace_and_mark_range(None, "x", None);
        assert_eq!(buf.content, "axb");
        assert_eq!(buf.marked_range, Some(1..2));
        buf.replace_and_mark_range(Some(1..2), "", None);
        assert_eq!(buf.content, "ab");
        assert_eq!(buf.marked_range, None);
    }

    #[test]
    fn a_plain_replace_range_always_clears_an_active_marked_range() {
        let mut buf = buffer("ab");
        buf.move_to(1);
        buf.replace_and_mark_range(None, "x", None);
        assert!(buf.marked_range.is_some());
        buf.replace_range(None, "y");
        assert_eq!(
            buf.marked_range, None,
            "a committed edit must end IME composition"
        );
    }

    /// CRITICAL regression coverage (finding 1): the real, live-reproduced panic an audit caught
    /// with real Japanese IME composition input. `new_selected_range_utf16` must be interpreted
    /// relative to `new_text` itself (the real platform contract - see
    /// `Self::replace_and_mark_range`'s own docs), never against the whole `self.content`. The
    /// pre-existing `replace_and_mark_range_records_a_real_marked_range_and_clears_on_empty_text`
    /// test above always passes `None` for this argument - exactly the one case that can never
    /// trigger this bug, which is why it survived until an audit constructed a real, non-`None`
    /// composing caret.
    #[test]
    fn replace_and_mark_range_computes_the_composition_selection_relative_to_new_text_not_the_whole_buffer(
    ) {
        let mut buf = buffer("prefix ok\n");
        buf.move_to("prefix ".len());
        // A real Japanese IME reports a composing caret 2 UTF-16 units into the composing text
        // itself ("\u{65e5}\u{672c}\u{8a9e}" - three real, non-ASCII characters, one UTF-16 unit
        // each) - i.e. right after "\u{65e5}\u{672c}".
        buf.replace_and_mark_range(None, "\u{65e5}\u{672c}\u{8a9e}", Some(2..2));
        assert_eq!(buf.content, "prefix \u{65e5}\u{672c}\u{8a9e}ok\n");

        let expected_offset = "prefix ".len() + "\u{65e5}\u{672c}".len();
        assert_eq!(
            buf.selected_range,
            expected_offset..expected_offset,
            "the composing caret must land 2 real chars into the composing text itself, not be \
             misread as an offset into the whole buffer (the old, buggy formula would have \
             landed inside \"prefix \" instead - byte offset 2 there falls mid-character in the \
             newly-composed text, exactly the non-char-boundary corruption that panicked on the \
             next edit)"
        );

        // The real, live-reproduced crash this fix addresses: ending the composition (a real
        // cancel/blur, not a further replace-and-mark) and then typing an ordinary character
        // must not panic. Before this fix, `selected_range` could be corrupted onto a
        // non-UTF-8-char-boundary byte offset here, and `String::replace_range` (inside the next
        // real edit) panics immediately on exactly that ("start of range should be a character
        // boundary").
        buf.unmark();
        buf.replace_range(None, "!");
        assert_eq!(buf.content, "prefix \u{65e5}\u{672c}!\u{8a9e}ok\n");
    }

    /// finding 7: pressing Backspace while a real IME composition is active must delete one real
    /// grapheme, not silently swallow the entire composing text - `Self::replace_range`'s own real
    /// priority (matching `EntityInputHandler::replace_text_in_range`'s documented contract)
    /// prefers `marked_range` over `selected_range` whenever both exist and `None` is passed, so
    /// `Self::backspace`/`Self::delete_forward` must pass their own just-computed
    /// `selected_range` explicitly rather than relying on that default.
    #[test]
    fn backspace_during_a_real_ime_composition_deletes_one_grapheme_not_the_whole_composition() {
        let mut buf = buffer("ab");
        buf.move_to(1);
        // Compose a real two-character CJK string mid-buffer.
        buf.replace_and_mark_range(None, "\u{65e5}\u{672c}", None);
        assert_eq!(buf.content, "a\u{65e5}\u{672c}b");
        assert!(buf.marked_range.is_some(), "composition should be active");

        buf.backspace();

        assert_eq!(
            buf.content, "a\u{65e5}b",
            "Backspace mid-composition must delete only the one real grapheme before the caret \
             (\u{672c}), not the entire composing text"
        );
    }

    /// finding 5: proves the incremental splice this fix introduces (`EditBuffer::splice_lines`)
    /// produces byte-for-byte identical `lines`/`line_ranges`/`utf16_line_starts` to a real,
    /// independent, from-scratch reconstruction (`EditBuffer::new` on the resulting content) -
    /// the strongest real correctness check available for logic this fiddly, run across a real
    /// variety of edit shapes: a line inserted at the very start, a real multi-byte
    /// emoji/CJK insertion mid-line, a delete that merges two real lines, a multi-line replace
    /// that crosses a line boundary, clearing and retyping the whole buffer, and appending past
    /// the real end of a buffer with no trailing newline.
    #[test]
    fn incremental_splicing_matches_a_real_independent_full_rebuild_across_many_real_edits() {
        fn assert_matches_full_rebuild(buf: &EditBuffer) {
            // `buf.lines`' real *text* must always match a real, independent rebuild - but its
            // run *classification* legitimately won't, for lines the edit never touched: unlike
            // the old whole-buffer `rebuild_plain`, `Self::splice_lines` only resets the touched
            // line(s) to plain `Text` (see that method's own docs), deliberately leaving an
            // untouched line's own prior *real* highlighting alone rather than needlessly
            // flickering it back to plain too - a real improvement, not a gap (the debounced
            // re-highlight still fully replaces every line's own runs regardless once it lands).
            let plain_lines = code_view::build_lines(&buf.content, &[]);
            let buf_texts: Vec<&str> = buf.lines.iter().map(|line| line.text.as_str()).collect();
            let plain_texts: Vec<&str> =
                plain_lines.iter().map(|line| line.text.as_str()).collect();
            assert_eq!(
                buf_texts, plain_texts,
                "line text diverged from a real, independent full rebuild; content: {:?}",
                buf.content
            );

            // `line_ranges`/`utf16_line_starts` have no such highlighting-dependence - a real,
            // independently-constructed `EditBuffer` is a genuine ground truth for both.
            let ground_truth = EditBuffer::new(
                buf.path.clone(),
                buf.content.clone(),
                buf.extension.clone(),
                None,
                buf.content.len() as u64,
            );
            assert_eq!(
                buf.line_ranges, ground_truth.line_ranges,
                "line_ranges diverged from a real, independent full rebuild; content: {:?}",
                buf.content
            );
            assert_eq!(
                buf.utf16_line_starts, ground_truth.utf16_line_starts,
                "utf16_line_starts diverged from a real, independent full rebuild; content: {:?}",
                buf.content
            );
        }

        // 1. Insert a real new line at the very start of the buffer.
        let mut buf = buffer("fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n");
        buf.move_to(0);
        buf.replace_range(None, "// a real comment header\n");
        assert_matches_full_rebuild(&buf);

        // 2. Insert a real multi-byte emoji + CJK string mid-line (also introduces a new line).
        let insert_at = buf.content.find("let x").expect("let x present");
        buf.move_to(insert_at);
        buf.replace_range(None, "// \u{1f600} \u{65e5}\u{672c}\u{8a9e}\n    ");
        assert_matches_full_rebuild(&buf);

        // 3. Delete across a real line boundary (merges two real lines into one).
        let mut buf2 = buffer("first line\nsecond line\nthird line\n");
        let newline = buf2.content.find("\nsecond").expect("newline present");
        buf2.selected_range = newline..newline + 1;
        buf2.replace_range(None, " ");
        assert_matches_full_rebuild(&buf2);
        assert_eq!(buf2.content, "first line second line\nthird line\n");

        // 4. A multi-line replace that crosses a real line boundary.
        let mut buf3 = buffer("one\ntwo\nthree\nfour\n");
        let start = buf3.content.find("two").expect("two present");
        let end = buf3.content.find("four").expect("four present");
        buf3.selected_range = start..end;
        buf3.replace_range(None, "REPLACED\nBLOCK\n");
        assert_matches_full_rebuild(&buf3);
        assert_eq!(buf3.content, "one\nREPLACED\nBLOCK\nfour\n");

        // 5. Clear the whole buffer, then retype real multi-line content from scratch.
        let mut buf4 = buffer("throwaway\ncontent\n");
        buf4.select_all();
        buf4.replace_range(None, "");
        assert_matches_full_rebuild(&buf4);
        buf4.replace_range(None, "fresh\nreal\ncontent\n");
        assert_matches_full_rebuild(&buf4);

        // 6. Append past the real end of a buffer with no trailing newline, then add one.
        let mut buf5 = buffer("no trailing newline yet");
        buf5.move_to(buf5.content.len());
        buf5.replace_range(None, " - more text");
        assert_matches_full_rebuild(&buf5);
        buf5.replace_range(None, "\n");
        assert_matches_full_rebuild(&buf5);

        // 7. Real CRLF content - the incremental splice must not disagree with a full rebuild's
        // own established CRLF handling either.
        let mut buf6 = buffer("crlf one\r\ncrlf two\r\ncrlf three\r\n");
        let start6 = buf6.content.find("two").expect("two present");
        buf6.selected_range = start6..start6 + "two".len();
        buf6.replace_range(None, "REPLACED");
        assert_matches_full_rebuild(&buf6);
    }

    /// finding 5: a real, measured before/after performance proof, following
    /// `code_view_cache_tests::opening_a_large_real_file_does_not_block_render_on_the_full_parse`'s
    /// own established "a ratio, not an absolute threshold" methodology (so it isn't flaky under
    /// CI load) - timed directly against this repo's own real, large `lsp/client.rs` file (its
    /// largest single source file - it was `root/code_surface.rs` before that file was split into
    /// this folder).
    /// `EditBuffer::rebuild_plain_full` (the previous, real whole-buffer-per-keystroke behavior)
    /// is still real, still callable (kept on as `Self::splice_lines`'s own defensive fallback),
    /// so this is a true apples-to-apples before/after comparison on the exact same real buffer,
    /// not a synthetic microbenchmark.
    #[test]
    fn a_real_incremental_edit_on_a_large_real_file_is_measurably_cheaper_than_a_real_whole_buffer_rebuild(
    ) {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lsp/client.rs"))
                .expect("read this crate's own lsp/client.rs as a real, large .rs fixture");
        let mut buf = buffer(&source);
        buf.move_to(source.len() / 2);

        const ITERATIONS: usize = 20;

        let full_rebuild_start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            buf.rebuild_plain_full();
        }
        let full_rebuild_duration = full_rebuild_start.elapsed();

        let incremental_start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            buf.replace_range(None, "x");
        }
        let incremental_duration = incremental_start.elapsed();

        assert!(
            incremental_duration < full_rebuild_duration,
            "{ITERATIONS} real single-character incremental edits on this repo's own large \
             {}-byte real .rs file took {incremental_duration:?} - not measurably less than \
             {ITERATIONS} real *whole-buffer* rebuilds on the exact same buffer \
             ({full_rebuild_duration:?}) - the incremental splice must not still be paying the \
             O(whole buffer) cost this fix targets",
            source.len(),
        );
    }

    // ------------------------------------------------------------------------------------------
    // GitHub issue #17 - real multi-step undo/redo. See this module's own top docs and
    // `crate::text_history`'s for the recorded shape and the coalescing policy these exercise.
    // ------------------------------------------------------------------------------------------

    /// Types `text` one real character at a time through the exact path a real keystroke takes
    /// (`EntityInputHandler::replace_text_in_range` -> `EditBuffer::replace_range`), with no
    /// intervening caret move - i.e. a genuine typing burst.
    fn type_text(buffer: &mut EditBuffer, text: &str) {
        for ch in text.chars() {
            buffer.replace_range(None, &ch.to_string());
        }
    }

    #[test]
    fn typing_several_characters_then_undo_restores_the_content_from_before_the_burst() {
        let mut buffer = buffer("fn main() {}\n");
        buffer.move_to(3);
        type_text(&mut buffer, "hello");
        assert_eq!(buffer.content, "fn hellomain() {}\n");

        assert!(buffer.undo(), "the burst must be undoable");
        assert_eq!(
            buffer.content, "fn main() {}\n",
            "five typed characters must undo as one coalesced step, not five"
        );
        assert!(
            !buffer.can_undo(),
            "and there must be nothing left behind it - proving it really was one step"
        );
    }

    #[test]
    fn undo_restores_the_real_caret_and_redo_restores_the_real_post_edit_caret() {
        let mut buffer = buffer("abcdef\n");
        buffer.move_to(3);
        type_text(&mut buffer, "XY");
        assert_eq!(buffer.cursor_offset(), 5);

        assert!(buffer.undo());
        assert_eq!(buffer.content, "abcdef\n");
        assert_eq!(
            buffer.cursor_offset(),
            3,
            "undo must put the caret back where the burst started, not leave it where the edit \
             ended"
        );

        assert!(buffer.redo());
        assert_eq!(buffer.content, "abcXYdef\n");
        assert_eq!(buffer.cursor_offset(), 5);
    }

    #[test]
    fn undo_restores_a_real_selection_not_just_a_collapsed_caret() {
        let mut buffer = buffer("abcdef\n");
        buffer.move_to(1);
        buffer.select_to(4);
        assert_eq!(buffer.selected_range, 1..4);
        // Typing over a real selection replaces it.
        buffer.replace_range(None, "Z");
        assert_eq!(buffer.content, "aZef\n");

        assert!(buffer.undo());
        assert_eq!(buffer.content, "abcdef\n");
        assert_eq!(
            buffer.selected_range,
            1..4,
            "undo must restore the real selection the edit replaced, not just a caret"
        );
    }

    #[test]
    fn a_reversed_selection_round_trips_through_undo_with_its_direction_intact() {
        let mut buffer = buffer("abcdef\n");
        buffer.move_to(4);
        buffer.select_to(1);
        assert!(buffer.selection_reversed);
        assert_eq!(buffer.cursor_offset(), 1);
        buffer.replace_range(None, "Z");

        assert!(buffer.undo());
        assert_eq!(buffer.selected_range, 1..4);
        assert!(
            buffer.selection_reversed,
            "the visible caret sat at the selection's start - undo must restore that, or the \
             next Shift+Arrow extends the wrong end"
        );
        assert_eq!(buffer.cursor_offset(), 1);
    }

    #[test]
    fn a_real_caret_jump_between_two_bursts_makes_them_two_separate_undo_steps() {
        let mut buffer = buffer("abcdef\n");
        buffer.move_to(0);
        type_text(&mut buffer, "XX");
        // A real arrow-key caret move - no time passes, but the caret is no longer where the
        // previous edit left it.
        buffer.move_right();
        buffer.move_right();
        type_text(&mut buffer, "YY");
        assert_eq!(buffer.content, "XXabYYcdef\n");

        assert!(buffer.undo());
        assert_eq!(
            buffer.content, "XXabcdef\n",
            "the second burst must undo on its own - a caret jump is a real group boundary"
        );
        assert!(buffer.undo());
        assert_eq!(buffer.content, "abcdef\n");
        assert!(!buffer.can_undo());
    }

    #[test]
    fn a_new_edit_after_an_undo_drops_the_redo_branch() {
        let mut buffer = buffer("abc\n");
        buffer.move_to(3);
        type_text(&mut buffer, "XX");
        assert!(buffer.undo());
        assert!(buffer.can_redo());

        type_text(&mut buffer, "Y");
        assert!(
            !buffer.can_redo(),
            "linear history: a fresh edit after an undo must discard the redo branch"
        );
        assert_eq!(buffer.content, "abcY\n");
    }

    #[test]
    fn consecutive_backspaces_undo_as_one_step_and_restore_the_pre_deletion_caret() {
        let mut buffer = buffer("abcdef\n");
        buffer.move_to(6);
        buffer.backspace();
        buffer.backspace();
        buffer.backspace();
        assert_eq!(buffer.content, "abc\n");
        assert_eq!(buffer.cursor_offset(), 3);

        assert!(buffer.undo());
        assert_eq!(buffer.content, "abcdef\n");
        assert_eq!(
            buffer.cursor_offset(),
            6,
            "the caret must return to where the deletion run started - the real bug that would \
             appear if `backspace` recorded the selection it extends over instead"
        );
        assert!(!buffer.can_undo());
    }

    #[test]
    fn deleting_forward_repeatedly_also_undoes_as_one_step() {
        let mut buffer = buffer("abcdef\n");
        buffer.move_to(2);
        buffer.delete_forward();
        buffer.delete_forward();
        assert_eq!(buffer.content, "abef\n");

        assert!(buffer.undo());
        assert_eq!(buffer.content, "abcdef\n");
        assert_eq!(buffer.cursor_offset(), 2);
        assert!(!buffer.can_undo());
    }

    #[test]
    fn a_real_ime_composition_commits_as_exactly_one_atomic_undo_step() {
        let mut buffer = buffer("x\n");
        buffer.move_to(1);

        // A real, multi-keystroke Japanese composition: three real `setMarkedText`-shaped updates
        // (each one replacing the previous marked range), then a real commit through the plain
        // `replace_text_in_range` path a platform IME uses to finish a composition. This is the
        // real sequence `crate::code_surface::editing`'s `EntityInputHandler` receives, not an
        // assumed one - see `replace_and_mark_range`'s own docs for the verified platform
        // contract.
        buffer.replace_and_mark_range(None, "\u{304b}", None);
        assert_eq!(buffer.content, "x\u{304b}\n");
        buffer.replace_and_mark_range(None, "\u{304b}\u{3093}", None);
        buffer.replace_and_mark_range(None, "\u{304b}\u{3093}\u{3058}", None);
        assert_eq!(buffer.content, "x\u{304b}\u{3093}\u{3058}\n");
        assert!(buffer.marked_range.is_some(), "sanity: still composing");
        buffer.replace_range(None, "\u{6f22}\u{5b57}");
        assert_eq!(buffer.content, "x\u{6f22}\u{5b57}\n");
        assert!(buffer.marked_range.is_none(), "sanity: composition ended");

        assert!(buffer.undo());
        assert_eq!(
            buffer.content, "x\n",
            "the whole composition - every intermediate update plus the commit - must be one \
             single undo step, never four"
        );
        assert_eq!(buffer.cursor_offset(), 1);
        assert!(!buffer.can_undo());

        assert!(buffer.redo());
        assert_eq!(buffer.content, "x\u{6f22}\u{5b57}\n");
    }

    /// Regression for a split found in self-review: a Backspace pressed *mid-composition* reaches
    /// `replace_range` with the composition still live, which clears `marked_range` as a side
    /// effect - and an earlier version sealed the group on that, so the rest of what the user
    /// experiences as one composition became a second undo step. Real platform IMEs routinely keep
    /// composing after a backspace inside the composing string.
    #[test]
    fn a_backspace_mid_composition_does_not_split_the_composition_into_two_undo_steps() {
        let mut buffer = buffer("x\n");
        buffer.move_to(1);

        buffer.replace_and_mark_range(None, "\u{304b}\u{3093}", None);
        assert_eq!(buffer.content, "x\u{304b}\u{3093}\n");
        // Backspace inside the composing string. `EditBuffer::replace_range` unconditionally
        // clears `marked_range` (Revision R8.5a behaviour, unchanged here), so as far as this
        // buffer is concerned the composition has ended - but the user is still mid-word, and the
        // platform carries right on sending composition updates.
        buffer.backspace();
        assert_eq!(buffer.content, "x\u{304b}\n");
        buffer.replace_and_mark_range(None, "\u{304b}\u{3044}", None);
        buffer.replace_range(None, "\u{6d77}");
        assert_eq!(buffer.content, "x\u{304b}\u{6d77}\n");

        assert!(buffer.undo());
        assert_eq!(
            buffer.content, "x\n",
            "everything from the first composition update through the mid-composition backspace \
             to the final commit must be one undo step - an earlier version sealed on the \
             backspace and split it in two"
        );
        assert!(!buffer.can_undo());
    }

    #[test]
    fn typing_after_a_composition_ends_is_a_separate_undo_step() {
        let mut buffer = buffer("\n");
        buffer.move_to(0);
        buffer.replace_and_mark_range(None, "\u{304b}", None);
        buffer.replace_range(None, "\u{6f22}");
        type_text(&mut buffer, "ab");
        assert_eq!(buffer.content, "\u{6f22}ab\n");

        assert!(buffer.undo());
        assert_eq!(
            buffer.content, "\u{6f22}\n",
            "the end of a composition is a hard boundary - the typing after it must not have \
             been absorbed into the composition's own step"
        );
        assert!(buffer.undo());
        assert_eq!(buffer.content, "\n");
    }

    /// Audit finding, reproduced end to end on a real buffer: a mid-composition Backspace
    /// deliberately leaves the `Ime` group open (so a continuing composition rejoins it), and
    /// `EditKind::Ime` coalescing ignores both the idle timeout and caret continuity - so an
    /// abandoned composition's group stayed open indefinitely and a completely unrelated
    /// composition somewhere else merged into it, one Ctrl+Z reverting both.
    ///
    /// The backspace deliberately removes only *part* of the preedit, so the abandoned group is
    /// genuinely not a net no-op and can't be disposed of by `EditGroup::is_net_noop` - this is
    /// specifically the caret-jump boundary under test, not the dead-step drop.
    #[test]
    fn two_unrelated_compositions_separated_by_a_caret_jump_are_two_undo_steps() {
        let mut buffer = buffer("abcdef\n");
        buffer.move_to(6);
        buffer.replace_and_mark_range(None, "\u{3042}\u{3044}", None);
        assert_eq!(buffer.content, "abcdef\u{3042}\u{3044}\n");
        // Backspace one grapheme out of the preedit: `marked_range` clears, but real composed
        // text is left behind, so the group is open *and* not identity.
        buffer.backspace();
        assert_eq!(buffer.content, "abcdef\u{3042}\n");
        assert!(buffer.marked_range.is_none());

        // A real caret jump, then a completely unrelated composition committed there.
        buffer.move_to(0);
        buffer.replace_and_mark_range(None, "\u{304b}", None);
        buffer.replace_range(None, "\u{6f22}");
        assert_eq!(buffer.content, "\u{6f22}abcdef\u{3042}\n");

        assert!(buffer.undo());
        assert_eq!(
            buffer.content, "abcdef\u{3042}\n",
            "undoing the second composition must revert only that composition - the abandoned \
             first one, on the other side of a real caret jump, is its own separate step"
        );
        assert!(
            buffer.can_undo(),
            "and that first composition must still be there to undo separately"
        );
        assert!(buffer.undo());
        assert_eq!(buffer.content, "abcdef\n");
    }

    #[test]
    fn a_cancelled_composition_is_still_a_hard_boundary() {
        let mut buffer = buffer("\n");
        buffer.move_to(0);
        buffer.replace_and_mark_range(None, "\u{304b}", None);
        // A real cancelled composition: the platform calls `unmark_text` without committing.
        buffer.unmark();
        type_text(&mut buffer, "ab");

        assert!(buffer.undo());
        assert_eq!(buffer.content, "\u{304b}\n");
        assert!(buffer.undo());
        assert_eq!(buffer.content, "\n");
    }

    #[test]
    fn seal_history_makes_a_paste_its_own_undo_step_in_both_directions() {
        let mut buffer = buffer("\n");
        buffer.move_to(0);
        type_text(&mut buffer, "ab");
        // Exactly what `AdeApp::handle_editor_paste_action` does around a real paste.
        buffer.seal_history();
        buffer.replace_range(None, "PASTED");
        buffer.seal_history();
        type_text(&mut buffer, "cd");
        assert_eq!(buffer.content, "abPASTEDcd\n");

        assert!(buffer.undo());
        assert_eq!(buffer.content, "abPASTED\n");
        assert!(buffer.undo());
        assert_eq!(buffer.content, "ab\n");
        assert!(buffer.undo());
        assert_eq!(buffer.content, "\n");
        assert!(!buffer.can_undo());
    }

    #[test]
    fn an_external_reload_is_one_undoable_step_and_never_a_silent_history_wipe() {
        let mut buffer = buffer("original\n");
        buffer.move_to(8);
        type_text(&mut buffer, "!");
        assert_eq!(buffer.content, "original!\n");
        // The user's own edit is saved, so the buffer is clean again against disk...
        buffer.mark_saved("original!\n".to_string(), None, 10);
        assert!(!buffer.is_dirty());

        // ...and now a real external writer (an agent CLI running in this worktree) rewrites it.
        let rewritten = "rewritten by an agent\n".to_string();
        let lines = code_view::build_lines(&rewritten, &[]);
        assert!(buffer.reload_from_disk(rewritten.clone(), lines, None, rewritten.len() as u64));
        assert_eq!(buffer.content, rewritten);
        assert!(!buffer.is_dirty(), "the reloaded buffer matches disk");

        // The reload is its own real step...
        assert!(buffer.undo());
        assert_eq!(
            buffer.content, "original!\n",
            "Ctrl+Z straight after an external reload must put the pre-reload content back"
        );
        // ...and everything recorded before it is still reachable - not wiped.
        assert!(
            buffer.can_undo(),
            "the history from before the reload must survive it - a silent wipe mid-stack is \
             exactly what GitHub issue #17 rules out"
        );
        assert!(buffer.undo());
        assert_eq!(buffer.content, "original\n");

        assert!(buffer.redo());
        assert_eq!(buffer.content, "original!\n");
        assert!(buffer.redo());
        assert_eq!(buffer.content, rewritten);
    }

    #[test]
    fn a_reload_whose_content_is_identical_records_no_step_at_all() {
        let mut buffer = buffer("same\n");
        let lines = code_view::build_lines("same\n", &[]);
        assert!(!buffer.reload_from_disk("same\n".to_string(), lines, None, 5));
        assert!(
            !buffer.can_undo(),
            "a reload that changes nothing must not push an empty step the user has to press \
             Ctrl+Z past"
        );
        assert_eq!(
            buffer.saved_len, 5,
            "but it must still refresh disk identity"
        );
    }

    #[test]
    fn a_reload_keeps_the_caret_in_bounds_when_the_new_content_is_shorter() {
        let mut buffer = buffer("a very long first line\n");
        buffer.move_to(20);
        let short = "hi\n".to_string();
        let lines = code_view::build_lines(&short, &[]);
        assert!(buffer.reload_from_disk(short.clone(), lines, None, short.len() as u64));
        assert!(
            buffer.cursor_offset() <= buffer.content.len(),
            "the caret must be clamped into the new content, never left dangling past its end"
        );
    }

    #[test]
    fn undo_and_redo_keep_the_incremental_line_tables_exactly_correct() {
        // The real risk a separate replay path would introduce: `undo`/`redo` splice through the
        // same `splice_lines` every edit uses, so the incremental `lines`/`line_ranges`/
        // `utf16_line_starts` tables must end up byte-identical to a fresh whole-buffer rebuild.
        let mut buffer = buffer("one\ntwo\nthree\n");
        buffer.move_to(4);
        type_text(&mut buffer, "X\nY");
        buffer.move_to(0);
        buffer.backspace();
        type_text(&mut buffer, "Z");
        assert!(buffer.undo());
        assert!(buffer.undo());
        assert!(buffer.redo());

        let expected = EditBuffer::new(
            PathBuf::from("/tmp/test.rs"),
            buffer.content.clone(),
            Some("rs".to_string()),
            None,
            buffer.content.len() as u64,
        );
        assert_eq!(buffer.line_ranges, expected.line_ranges);
        assert_eq!(buffer.utf16_line_starts, expected.utf16_line_starts);
        assert_eq!(
            buffer.lines.len(),
            expected.lines.len(),
            "the same number of real rendered rows as a fresh, independent rebuild"
        );
        for (index, (actual, wanted)) in buffer.lines.iter().zip(expected.lines.iter()).enumerate()
        {
            let actual_text: String = actual.runs.iter().map(|run| run.0.to_string()).collect();
            let wanted_text: String = wanted.runs.iter().map(|run| run.0.to_string()).collect();
            assert_eq!(actual_text, wanted_text, "row {index}");
        }
    }

    #[test]
    fn undo_is_refused_rather_than_corrupting_content_a_group_no_longer_describes() {
        let mut buffer = buffer("abc\n");
        buffer.move_to(3);
        type_text(&mut buffer, "XY");
        // Deliberately desynchronize the history from the content, the way only a bug could:
        // rewrite `content` behind the buffer's own back.
        buffer.content = "completely different\n".to_string();
        assert!(
            !buffer.undo(),
            "a group that doesn't describe the current bytes must be refused outright"
        );
        assert_eq!(
            buffer.content, "completely different\n",
            "and the content must be left exactly as it was, not half-spliced"
        );
        assert!(
            buffer.can_undo() && !buffer.can_redo(),
            "the history cursor must not have moved either - a refusal that still stepped the \
             cursor would leave the stack silently desynchronized from the content, which is \
             exactly what the validation exists to prevent"
        );
    }

    #[test]
    fn undo_and_redo_on_an_empty_history_are_honest_no_ops() {
        let mut buffer = buffer("abc\n");
        assert!(!buffer.can_undo());
        assert!(!buffer.can_redo());
        assert!(!buffer.undo());
        assert!(!buffer.redo());
        assert_eq!(buffer.content, "abc\n");
    }

    // ---- Multi-cursor (Revision R13, issue #28) ----

    #[test]
    fn word_range_at_finds_the_real_word_touching_either_side_of_the_caret() {
        let buf = buffer("foo bar_baz\n");
        // Caret in the middle of "foo".
        assert_eq!(buf.word_range_at(1), Some(0..3));
        // Caret right before "bar_baz" - touches only on the right.
        assert_eq!(buf.word_range_at(4), Some(4..11));
        // Caret right after "foo" - touches only on the left.
        assert_eq!(buf.word_range_at(3), Some(0..3));
        // Underscore counts as a word character - the whole identifier is one word.
        assert_eq!(buf.word_range_at(8), Some(4..11));
    }

    #[test]
    fn word_range_at_is_none_with_no_adjacent_word_character() {
        let buf = buffer("foo   bar\n");
        // Deep inside the run of spaces, touching neither word.
        assert_eq!(buf.word_range_at(5), None);
    }

    #[test]
    fn ctrl_d_first_press_selects_the_real_word_under_an_empty_caret() {
        let mut buf = buffer("let value = value + 1;\n");
        buf.move_to(6); // inside the first "value"
        assert!(buf.select_word_or_add_next_occurrence());
        assert_eq!(buf.selected_range, 4..9);
        assert_eq!(&buf.content[buf.selected_range.clone()], "value");
        assert!(buf.secondary_cursors.is_empty());
    }

    #[test]
    fn ctrl_d_second_press_adds_the_next_real_occurrence_as_a_new_cursor() {
        let mut buf = buffer("value + value + value\n");
        buf.move_to(1); // inside the first "value"
        assert!(buf.select_word_or_add_next_occurrence()); // selects the first "value"
        assert!(buf.select_word_or_add_next_occurrence()); // adds the second "value"

        assert_eq!(buf.cursor_count(), 2);
        let mut ranges = buf.all_selection_ranges();
        ranges.sort_by_key(|range| range.start);
        assert_eq!(ranges, vec![0..5, 8..13]);
        // The newest occurrence becomes primary.
        assert_eq!(buf.selected_range, 8..13);
    }

    #[test]
    fn ctrl_d_keeps_adding_occurrences_and_wraps_around_the_buffer() {
        let mut buf = buffer("value + value + other\n");
        buf.move_to(1);
        assert!(buf.select_word_or_add_next_occurrence()); // first "value" (0..5)
        assert!(buf.select_word_or_add_next_occurrence()); // second "value" (8..13)
                                                           // No more real, not-already-selected occurrences of "value" anywhere in the buffer - a
                                                           // real no-op, not a fabricated third cursor.
        assert!(!buf.select_word_or_add_next_occurrence());
        assert_eq!(buf.cursor_count(), 2);
    }

    #[test]
    fn ctrl_d_occurrence_search_is_case_sensitive() {
        let mut buf = buffer("Value value\n");
        buf.move_to(0);
        assert!(buf.select_word_or_add_next_occurrence()); // selects "Value" (0..5)
                                                           // The only other real occurrence in the buffer is lowercase "value" - must not match a
                                                           // differently-cased real occurrence, matching VS Code's own default case-sensitive search.
        assert!(!buf.select_word_or_add_next_occurrence());
        assert_eq!(buf.cursor_count(), 1);
    }

    #[test]
    fn ctrl_shift_l_selects_every_real_occurrence_at_once() {
        let mut buf = buffer("value + value + value\n");
        buf.move_to(1);
        assert!(buf.select_all_occurrences());
        assert_eq!(buf.cursor_count(), 3);
        let mut ranges = buf.all_selection_ranges();
        ranges.sort_by_key(|range| range.start);
        assert_eq!(ranges, vec![0..5, 8..13, 16..21]);
    }

    #[test]
    fn ctrl_k_ctrl_d_skips_the_current_occurrence_without_keeping_it_selected() {
        let mut buf = buffer("value + value + value\n");
        buf.move_to(1);
        assert!(buf.select_word_or_add_next_occurrence()); // selects the first "value" (0..5)
        assert!(buf.skip_current_occurrence()); // moves to the second, not keeping the first
        assert_eq!(buf.cursor_count(), 1, "skip must not add a cursor");
        assert_eq!(buf.selected_range, 8..13);
    }

    #[test]
    fn escape_collapses_every_secondary_cursor_back_to_one() {
        let mut buf = buffer("value + value\n");
        buf.move_to(1);
        buf.select_word_or_add_next_occurrence();
        buf.select_word_or_add_next_occurrence();
        assert_eq!(buf.cursor_count(), 2);

        assert!(buf.collapse_to_single_cursor());
        assert_eq!(buf.cursor_count(), 1);
        // A second Escape with only one cursor left is a genuine no-op.
        assert!(!buf.collapse_to_single_cursor());
    }

    #[test]
    fn add_cursor_at_adds_an_arbitrary_empty_cursor_keeping_the_existing_one() {
        let mut buf = buffer("abcdef\n");
        buf.move_to(0);
        buf.add_cursor_at(4);
        assert_eq!(buf.cursor_count(), 2);
        let mut ranges = buf.all_selection_ranges();
        ranges.sort_by_key(|range| range.start);
        assert_eq!(ranges, vec![0..0, 4..4]);
        // The newest (just-clicked) position becomes primary.
        assert_eq!(buf.selected_range, 4..4);
    }

    #[test]
    fn typing_with_multiple_cursors_inserts_the_same_text_at_every_cursor_as_one_atomic_edit() {
        let mut buf = buffer("value + value\n");
        buf.move_to(1);
        buf.select_word_or_add_next_occurrence();
        buf.select_word_or_add_next_occurrence();
        assert_eq!(buf.cursor_count(), 2);

        buf.replace_range(None, "x");

        assert_eq!(
            buf.content, "x + x\n",
            "typing must replace the real selected text at every cursor at once"
        );
        // Every cursor collapses to an empty caret right after its own inserted text.
        let mut ranges = buf.all_selection_ranges();
        ranges.sort_by_key(|range| range.start);
        assert_eq!(ranges, vec![1..1, 5..5]);
    }

    #[test]
    fn pasting_with_multiple_cursors_applies_to_every_cursor() {
        // `EntityInputHandler::replace_text_in_range` (paste's own real call path) passes `None`
        // for an ordinary paste with no explicit range - this proves the exact same real code
        // path `AdeApp::handle_editor_paste_action` uses fans out across every cursor.
        let mut buf = buffer("a a\n");
        buf.selected_range = 0..1; // select the first "a"
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 2..3, // select the second "a"
            reversed: false,
        }];
        buf.replace_range(None, "bb");
        assert_eq!(buf.content, "bb bb\n");
    }

    #[test]
    fn backspace_with_multiple_cursors_deletes_one_grapheme_at_every_cursor() {
        let mut buf = buffer("aXbXc\n");
        buf.selected_range = 2..2; // right after the first X
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 4..4, // right after the second X
            reversed: false,
        }];
        buf.backspace();
        assert_eq!(buf.content, "abc\n");
        let mut ranges = buf.all_selection_ranges();
        ranges.sort_by_key(|range| range.start);
        assert_eq!(ranges, vec![1..1, 2..2]);
    }

    #[test]
    fn backspace_with_multiple_cursors_is_a_real_per_cursor_no_op_at_the_start_of_the_buffer() {
        // One cursor sits at the very start of the buffer (nothing to delete for it); the other
        // has real text before it. The first must not block or corrupt the second's own real
        // deletion.
        let mut buf = buffer("Xbc\n");
        buf.selected_range = 0..0;
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 2..2, // right after "b"
            reversed: false,
        }];
        buf.backspace();
        assert_eq!(buf.content, "Xc\n");
    }

    #[test]
    fn delete_forward_with_multiple_cursors_deletes_one_grapheme_at_every_cursor() {
        let mut buf = buffer("aXbXc\n");
        buf.selected_range = 1..1; // right before the first X
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 3..3, // right before the second X
            reversed: false,
        }];
        buf.delete_forward();
        assert_eq!(buf.content, "abc\n");
    }

    #[test]
    fn colliding_cursors_merge_into_one_after_an_edit() {
        // Two carets one grapheme apart; deleting forward from the left one lands them both on
        // the exact same offset - they must merge into a single real cursor, not stay duplicated.
        let mut buf = buffer("aXc\n");
        buf.selected_range = 1..1; // right before X
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 1..1, // an already-identical cursor, simplest real collision case
            reversed: false,
        }];
        buf.delete_forward();
        assert_eq!(buf.content, "ac\n");
        assert_eq!(
            buf.cursor_count(),
            1,
            "two cursors landing on the exact same offset must merge"
        );
    }

    #[test]
    fn overlapping_selections_merge_into_one_real_cursor() {
        let mut buf = buffer("abcdef\n");
        buf.selected_range = 0..3;
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 2..5,
            reversed: false,
        }];
        // Any real operation that runs `Self::merge_colliding_cursors` picks this up - `Ctrl+D`'s
        // own no-op-when-nothing-to-search path doesn't touch cursors, so use `Self::add_cursor_at`
        // (itself always merges) with a position that doesn't introduce a *third* collision.
        buf.add_cursor_at(6);
        assert_eq!(
            buf.cursor_count(),
            2,
            "the two overlapping ranges merge into one"
        );
        let mut ranges = buf.all_selection_ranges();
        ranges.sort_by_key(|range| range.start);
        assert_eq!(ranges, vec![0..5, 6..6]);
    }

    #[test]
    fn touching_but_non_overlapping_selections_do_not_merge() {
        let mut buf = buffer("abcdef\n");
        buf.selected_range = 0..3;
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 3..6, // touches at byte 3, but does not overlap
            reversed: false,
        }];
        buf.merge_colliding_cursors();
        assert_eq!(
            buf.cursor_count(),
            2,
            "merely touching (not overlapping) selections stay distinct"
        );
    }

    #[test]
    fn arrow_keys_move_every_real_cursor_together() {
        let mut buf = buffer("abc abc\n");
        buf.selected_range = 0..0;
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 4..4,
            reversed: false,
        }];
        buf.move_right();
        let mut ranges = buf.all_selection_ranges();
        ranges.sort_by_key(|range| range.start);
        assert_eq!(
            ranges,
            vec![1..1, 5..5],
            "Right must move both real cursors forward one grapheme each, not just the primary"
        );
    }

    #[test]
    fn a_plain_click_move_to_collapses_every_secondary_cursor() {
        let mut buf = buffer("abcdef\n");
        buf.selected_range = 0..0;
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 4..4,
            reversed: false,
        }];
        buf.move_to(2);
        assert_eq!(
            buf.cursor_count(),
            1,
            "an ordinary, unmodified click always collapses back to a single real cursor"
        );
        assert_eq!(buf.selected_range, 2..2);
    }

    #[test]
    fn select_all_collapses_every_secondary_cursor_into_one_whole_buffer_selection() {
        let mut buf = buffer("abcdef\n");
        buf.selected_range = 0..0;
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 4..4,
            reversed: false,
        }];
        buf.select_all();
        assert_eq!(buf.cursor_count(), 1);
        assert_eq!(buf.selected_range, 0..buf.content.len());
    }

    // ---- Multi-cursor edits recorded as one real undo step (issue #28, on top of issue #17) ----

    #[test]
    fn typing_at_multiple_cursors_then_undo_restores_all_of_them_as_one_step() {
        let mut buf = buffer("value + value\n");
        buf.move_to(1);
        buf.select_word_or_add_next_occurrence();
        buf.select_word_or_add_next_occurrence();
        assert_eq!(buf.cursor_count(), 2);

        buf.replace_range(None, "x");
        assert_eq!(buf.content, "x + x\n");

        assert!(buf.undo(), "a multi-cursor edit must be undoable");
        assert_eq!(
            buf.content, "value + value\n",
            "undo must restore the text at every cursor's own edit, not just one"
        );
        assert!(
            !buf.can_undo(),
            "the whole multi-cursor edit must have been exactly one undo step, not two"
        );
    }

    #[test]
    fn redo_after_undoing_a_multi_cursor_edit_reapplies_every_cursors_own_edit() {
        let mut buf = buffer("value + value\n");
        buf.move_to(1);
        buf.select_word_or_add_next_occurrence();
        buf.select_word_or_add_next_occurrence();
        buf.replace_range(None, "x");
        assert!(buf.undo());
        assert_eq!(buf.content, "value + value\n");

        assert!(buf.redo());
        assert_eq!(
            buf.content, "x + x\n",
            "redo must reapply every cursor's own edit together, not just one"
        );
    }

    #[test]
    fn undo_after_a_multi_cursor_edit_leaves_the_buffer_in_ordinary_single_cursor_state() {
        // A real, documented limitation - see `EditBuffer`'s own "Multi-cursor edits are one real
        // undo step" docs: `text_history::SelectionSnapshot` has no way to represent more than one
        // cursor, so undo restores the text at every cursor but only the *primary*'s own
        // caret/selection - every secondary is dropped, not left stale.
        let mut buf = buffer("value + value\n");
        buf.move_to(1);
        buf.select_word_or_add_next_occurrence();
        buf.select_word_or_add_next_occurrence();
        buf.replace_range(None, "x");
        assert_eq!(
            buf.cursor_count(),
            2,
            "typing collapses each real cursor to its own empty caret, not down to a single \
             global one - there are still two real cursors here, one per edit"
        );

        assert!(buf.undo());
        assert_eq!(
            buf.cursor_count(),
            1,
            "undo must leave the buffer in ordinary single-cursor state, not a stale secondary"
        );
    }

    #[test]
    fn backspace_at_multiple_cursors_then_undo_restores_the_deleted_text_at_every_cursor() {
        let mut buf = buffer("aXbXc\n");
        buf.selected_range = 2..2; // right after the first X
        buf.secondary_cursors = vec![SecondaryCursor {
            range: 4..4, // right after the second X
            reversed: false,
        }];
        buf.backspace();
        assert_eq!(buf.content, "abc\n");

        assert!(buf.undo());
        assert_eq!(
            buf.content, "aXbXc\n",
            "undo must restore both deleted characters, not just one"
        );
    }

    #[test]
    fn consecutive_multi_cursor_keystrokes_coalesce_into_one_undo_group() {
        let original = "v + v\n";
        let mut buf = buffer(original);
        buf.move_to(0);
        buf.add_cursor_at(4);
        assert_eq!(buf.cursor_count(), 2);

        // Three real keystrokes in a row, no intervening caret move - a genuine typing burst
        // across both cursors, exactly like this module's own single-cursor `type_text` helper
        // drives for the plain coalescing tests above.
        buf.replace_range(None, "a");
        buf.replace_range(None, "b");
        buf.replace_range(None, "c");
        assert_ne!(buf.content, original);

        assert!(buf.undo(), "the whole burst must be undoable");
        assert_eq!(
            buf.content, original,
            "three keystrokes across two cursors must undo as one coalesced step, not six"
        );
        assert!(
            !buf.can_undo(),
            "and there must be nothing left behind it - proving it really was one group"
        );
    }

    // GitHub issue #26: `Tab`/`Shift+Tab` indentation.

    #[test]
    fn indent_lines_with_no_selection_inserts_at_the_caret_like_ordinary_typing() {
        let mut buf = buffer("fn main() {\nbody\n}\n");
        buf.move_to(12); // "fn main() {\n" is 12 bytes - this is the real start of "body"
        assert!(buf.indent_lines("  "));
        assert_eq!(buf.content, "fn main() {\n  body\n}\n");
        assert_eq!(buf.selected_range, 14..14);
    }

    #[test]
    fn indent_lines_with_a_multi_line_selection_indents_every_touched_line_and_keeps_selecting() {
        let mut buf = buffer("one\ntwo\nthree\n");
        buf.selected_range = 1..6; // "ne\ntw" - touches lines 0 and 1
        buf.selection_reversed = false;
        assert!(buf.indent_lines("  "));
        assert_eq!(buf.content, "  one\n  two\nthree\n");
        // The selection re-expands to the full touched lines' own new start through the shifted
        // original end (2 lines * 2 inserted spaces each = +4 total by the original end offset).
        assert_eq!(buf.selected_range, 0..10);
        assert_eq!(&buf.content[buf.selected_range.clone()], "  one\n  tw");
    }

    #[test]
    fn indent_lines_excludes_the_trailing_line_when_selection_ends_at_its_column_zero() {
        let mut buf = buffer("one\ntwo\nthree\n");
        buf.selected_range = 0..4; // "one\n" - lands exactly at the start of line 1
        assert!(buf.indent_lines("  "));
        assert_eq!(
            buf.content, "  one\ntwo\nthree\n",
            "only line 0 should be indented, not line 1"
        );
    }

    #[test]
    fn indent_lines_with_a_single_line_selection_still_block_indents_the_whole_line() {
        let mut buf = buffer("hello world\n");
        buf.selected_range = 6..11; // "world", no line break involved at all
        assert!(buf.indent_lines("\t"));
        assert_eq!(buf.content, "\thello world\n");
    }

    #[test]
    fn dedent_lines_removes_up_to_tab_width_of_leading_spaces_from_every_touched_line() {
        // Line 0 has fewer leading spaces than `width` (all of them are removed); line 1 has
        // more (only `width` of them are removed, leaving the rest); line 2 has none (no-op).
        let mut buf = buffer("  one\n      two\nthree\n");
        buf.selected_range = 0..buf.content.len();
        assert!(buf.dedent_lines(4));
        assert_eq!(buf.content, "one\n  two\nthree\n");
    }

    #[test]
    fn dedent_lines_removes_a_single_leading_tab_as_one_whole_level() {
        let mut buf = buffer("\t\tone\n");
        buf.move_to(3);
        assert!(buf.dedent_lines(4));
        assert_eq!(buf.content, "\tone\n");
    }

    #[test]
    fn dedent_lines_is_a_real_no_op_on_a_line_already_at_column_zero() {
        let mut buf = buffer("one\ntwo\n");
        buf.selected_range = 0..0;
        assert!(!buf.dedent_lines(4));
        assert_eq!(buf.content, "one\ntwo\n");
    }

    #[test]
    fn dedent_lines_with_no_selection_dedents_the_current_line_regardless_of_caret_column() {
        let mut buf = buffer("    hello\n");
        buf.move_to(7); // caret mid-word ("hel|lo"), nowhere near the leading whitespace
        assert!(buf.dedent_lines(4));
        assert_eq!(buf.content, "hello\n");
    }

    #[test]
    fn indent_then_dedent_round_trips_back_to_the_original_content() {
        let mut buf = buffer("one\ntwo\nthree\n");
        buf.selected_range = 0..buf.content.len();
        assert!(buf.indent_lines("    "));
        assert!(buf.dedent_lines(4));
        assert_eq!(buf.content, "one\ntwo\nthree\n");
    }

    // GitHub issue #17 integration: a multi-line Tab/Shift+Tab is one atomic undo step.

    #[test]
    fn indenting_three_lines_at_once_undoes_in_a_single_ctrl_z() {
        let mut buf = buffer("one\ntwo\nthree\n");
        buf.selected_range = 0..buf.content.len();
        assert!(buf.indent_lines("  "));
        assert_eq!(buf.content, "  one\n  two\n  three\n");

        assert!(buf.undo());
        assert_eq!(
            buf.content, "one\ntwo\nthree\n",
            "indenting all three lines must be exactly one undo step, not three"
        );
        assert!(
            !buf.can_undo(),
            "and there must be nothing left behind it - proving it really was one step"
        );

        assert!(buf.redo());
        assert_eq!(buf.content, "  one\n  two\n  three\n");
    }

    #[test]
    fn dedenting_three_lines_at_once_undoes_in_a_single_ctrl_z() {
        let mut buf = buffer("  one\n  two\n  three\n");
        buf.selected_range = 0..buf.content.len();
        assert!(buf.dedent_lines(2));
        assert_eq!(buf.content, "one\ntwo\nthree\n");

        assert!(buf.undo());
        assert_eq!(
            buf.content, "  one\n  two\n  three\n",
            "dedenting all three lines must be exactly one undo step, not three"
        );
        assert!(!buf.can_undo());

        assert!(buf.redo());
        assert_eq!(buf.content, "one\ntwo\nthree\n");
    }

    #[test]
    fn a_dedent_that_only_touches_some_lines_still_undoes_as_one_step() {
        // Line 1 has no leading whitespace at all - a genuine per-line no-op mixed in with two
        // real removals, exercising the "skip lines with nothing to remove" path inside one group.
        let mut buf = buffer("  one\ntwo\n  three\n");
        buf.selected_range = 0..buf.content.len();
        assert!(buf.dedent_lines(2));
        assert_eq!(buf.content, "one\ntwo\nthree\n");

        assert!(buf.undo());
        assert_eq!(buf.content, "  one\ntwo\n  three\n");
        assert!(!buf.can_undo());
    }
}
