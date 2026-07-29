//! Pure logic for Surface C's File view real text editing (Revision R8.5a): an [`EditBuffer`]
//! holds one open file's live, in-memory edited text plus a real cursor/selection/IME-composition
//! state, and every mutation (typing, IME composition, Backspace/Delete/Enter, arrow-key/click
//! cursor movement) goes through it. Deliberately `gpui`-window-free (only `gpui::SharedString`,
//! transitively via [`code_view::RenderedLine`], is used, for plain highlighted-text data),
//! mirroring `crate::code_view`'s own split between pure logic and `crate::root`'s live `Div`/
//! `Window` construction - see that module's own top doc comment for the same convention. The
//! real GPUI wiring (`EntityInputHandler`, keyboard actions, painting a real cursor/selection)
//! lives in `crate::root::editing`, which drives this module's methods.
//!
//! ## No undo/redo yet - a documented, deliberate gap
//!
//! This buffer keeps no edit history at all - every [`Self::replace_range`]/
//! [`Self::replace_and_mark_range`] call is destructive, with no way to step backward. That's a
//! real, intentional scope cut for this phase (Revision R8.5a), not an oversight: a correct
//! undo/redo stack (grouping keystrokes into coalesced edits, surviving re-highlighting, playing
//! well with an external-change conflict) is real, substantial design work of its own, and is
//! explicitly out of scope here per this phase's own instructions - it's Revision R10's job.
//!
//! ## Diff/Merge views stay 100% read-only
//!
//! Only the File view gets real editing this phase. `crate::root::code_surface::
//! render_diff_file_detail` and `crate::root::merge_flow_render`'s conflict-resolution columns
//! are untouched - neither renders through [`EditBuffer`] or gains any `EntityInputHandler`
//! wiring, so a hand-edit inside a diff hunk or an in-progress merge conflict resolution is still
//! not possible, exactly as before this phase.
//!
//! ## Re-highlighting cost, and why typing doesn't re-run `tree-sitter` on every keystroke
//!
//! A real, measured number, not a guess: highlighting this crate's own largest real `.rs` file
//! (`crates/app/src/root/code_surface.rs`, 4361 lines / ~195KB) end to end
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
use std::time::SystemTime;

use unicode_segmentation::UnicodeSegmentation;

use crate::code_view;
use crate::language::HighlighterFn;

/// One open file's real, live-edited text plus real cursor/selection/IME-composition state - see
/// this module's own docs. `selected_range`/`marked_range` are byte offsets into [`Self::content`]
/// (never UTF-16 code units - that conversion happens only at `crate::root::editing`'s
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
    /// `crate::root::editing`'s external-change-conflict check compares a fresh
    /// `std::fs::metadata` read against this, not against `crate::root::AdeApp::file_view_cache`
    /// (which is throttled and serves a different purpose - see that field's own docs).
    pub saved_mtime: Option<SystemTime>,
    /// The real on-disk length paired with [`Self::saved_mtime`] - same real-metadata-at-load-or-
    /// save-time discipline as `code_view::ParsedFile::len`.
    pub saved_len: u64,
    /// The real, currently-displayed per-line syntax-highlighted content - reused directly by
    /// `crate::root::render_file_view`'s row builder exactly like `code_view::ParsedFile::lines`
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
    /// straight vertical caret; see `crate::root::editing`'s own docs for this documented scope
    /// decision), preserved across a consecutive run of `Self::move_up`/`Self::move_down` and
    /// cleared by every other cursor-moving action - standard editor "sticky column" behavior.
    pub goal_column: Option<usize>,
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
    /// write (see `crate::root::editing::AdeApp::spawn_file_save_loop`'s docs for why this is read
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
    /// on this repo's own real 211KB `code_surface.rs`).
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
    pub fn move_to(&mut self, offset: usize) {
        let offset = self.floor_char_boundary(offset.min(self.content.len()));
        self.selected_range = offset..offset;
        self.selection_reversed = false;
    }

    /// Extends the selection to `offset` from whichever end is currently anchored, flipping
    /// [`Self::selection_reversed`] if the selection crosses over itself - ported from
    /// `vendor/zed/crates/gpui/examples/input.rs`'s own `TextInput::select_to`.
    pub fn select_to(&mut self, offset: usize) {
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
    /// ~200 KiB `code_surface.rs` in a release build, i.e. up to tens of milliseconds per single
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
    /// addresses text in UTF-16 code units (`crate::root::editing`'s `EntityInputHandler` impl is
    /// the only real caller; every other method on this type stays in UTF-8 bytes) - ported in
    /// spirit from `vendor/zed/crates/gpui/examples/input.rs`, but no longer a whole-buffer scan:
    /// a real, measured performance bug an audit caught (~122µs/call, release, on this repo's own
    /// real 211KB `code_surface.rs` - a real cost paid up to twice per real platform IME keystroke,
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
        let range = range
            .map(|range| self.clamp_range(range))
            .or_else(|| self.marked_range.clone())
            // Defensively clamped too - see this method's own docs on why `self.selected_range`
            // itself is never trusted blindly as a splice target, however it got here.
            .unwrap_or_else(|| self.clamp_range(self.selected_range.clone()));
        self.splice_lines(range.clone(), new_text);
        let new_pos = range.start + new_text.len();
        self.selected_range = new_pos..new_pos;
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_column = None;
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
    }

    /// Ends an IME composition without committing it as a real edit (a cancelled/dismissed
    /// composition) - the marked text itself was already spliced into `content` by whichever
    /// [`Self::replace_and_mark_range`] call is being unmarked, so this only clears the marker.
    pub fn unmark(&mut self) {
        self.marked_range = None;
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
        if self.selected_range.is_empty() {
            let previous = self.previous_boundary(self.cursor_offset());
            if previous == self.cursor_offset() {
                return;
            }
            self.select_to(previous);
        }
        self.replace_range(Some(self.selected_range.clone()), "");
    }

    /// `Delete`: the mirror of [`Self::backspace`] - see its own docs for why the real, just-
    /// computed [`Self::selected_range`] is passed explicitly rather than `None`.
    pub fn delete_forward(&mut self) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if next == self.cursor_offset() {
                return;
            }
            self.select_to(next);
        }
        self.replace_range(Some(self.selected_range.clone()), "");
    }

    /// `Left`: collapses a real selection to its start, or moves the caret one real grapheme
    /// left. Clears [`Self::goal_column`] (a horizontal move).
    pub fn move_left(&mut self) {
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
        self.goal_column = None;
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            self.move_to(next);
        } else {
            self.move_to(self.selected_range.end);
        }
    }

    /// `Shift+Left`: extends the selection one real grapheme left.
    pub fn select_left(&mut self) {
        self.goal_column = None;
        let previous = self.previous_boundary(self.cursor_offset());
        self.select_to(previous);
    }

    /// `Shift+Right`: extends the selection one real grapheme right.
    pub fn select_right(&mut self) {
        self.goal_column = None;
        let next = self.next_boundary(self.cursor_offset());
        self.select_to(next);
    }

    /// `Ctrl/Cmd+A`: selects the whole buffer.
    pub fn select_all(&mut self) {
        self.goal_column = None;
        self.move_to(0);
        self.select_to(self.content.len());
    }

    /// `Home`: moves the caret to the start of its current line.
    pub fn move_home(&mut self) {
        self.goal_column = None;
        let (line, _) = self.line_col_for_offset(self.cursor_offset());
        self.move_to(self.offset_for_line_col(line, 0));
    }

    /// `End`: moves the caret to the end of its current line's real text (before any line-ending
    /// bytes).
    pub fn move_end(&mut self) {
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

    /// `Up`: moves the caret to the previous line, at the real remembered goal column.
    pub fn move_up(&mut self) {
        let target = self.vertical_offset(-1);
        let goal = self.goal_column;
        self.move_to(target);
        self.goal_column = goal;
    }

    /// `Down`: the mirror of [`Self::move_up`].
    pub fn move_down(&mut self) {
        let target = self.vertical_offset(1);
        let goal = self.goal_column;
        self.move_to(target);
        self.goal_column = goal;
    }

    /// `Shift+Up`: extends the selection to the previous line at the real remembered goal column.
    pub fn select_up(&mut self) {
        let target = self.vertical_offset(-1);
        let goal = self.goal_column;
        self.select_to(target);
        self.goal_column = goal;
    }

    /// `Shift+Down`: the mirror of [`Self::select_up`].
    pub fn select_down(&mut self) {
        let target = self.vertical_offset(1);
        let goal = self.goal_column;
        self.select_to(target);
        self.goal_column = goal;
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
    /// at all. `crate::root::editing`'s per-row painter reads this for every visible row the
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

    /// The real caret position local to `line`'s text - `None` when there's an active selection
    /// (no caret bar is drawn then, matching `vendor/zed/crates/gpui/examples/input.rs`'s own
    /// `TextElement`) or the caret isn't on `line` at all.
    pub fn cursor_within_line(&self, line: usize) -> Option<usize> {
        if !self.selected_range.is_empty() {
            return None;
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(content: &str) -> EditBuffer {
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
    /// CI load) - timed directly against this repo's own real, large `root/code_surface.rs` file.
    /// `EditBuffer::rebuild_plain_full` (the previous, real whole-buffer-per-keystroke behavior)
    /// is still real, still callable (kept on as `Self::splice_lines`'s own defensive fallback),
    /// so this is a true apples-to-apples before/after comparison on the exact same real buffer,
    /// not a synthetic microbenchmark.
    #[test]
    fn a_real_incremental_edit_on_a_large_real_file_is_measurably_cheaper_than_a_real_whole_buffer_rebuild(
    ) {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/root/code_surface.rs"
        ))
        .expect("read this crate's own root/code_surface.rs as a real, large .rs fixture");
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
}
