//! ANSI/VT100 terminal grid emulation via `alacritty_terminal::Term`.

use crate::terminal::mouse::{MouseEncoding, MouseProtocol, MouseTracking};
use crate::terminal::osc::{OscWatcher, Progress};
use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll as AlacScroll};
use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, StdSyncHandler};

/// A concrete [`Dimensions`] impl for a fixed rows/cols grid. Scrollback history is reported as
/// equal to the visible screen (`total_lines == screen_lines`) since this module doesn't expose
/// scrollback UI (see the module docs).
#[derive(Debug, Clone, Copy)]
struct GridSize {
    rows: usize,
    cols: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Captures the two [`AlacEvent`]s this app actually acts on - [`AlacEvent::PtyWrite`] and the
/// window-title pair [`AlacEvent::Title`]/[`AlacEvent::ResetTitle`] - and drops every other one
/// (bell, clipboard, cursor-blink requests, ...): see the module docs' API surface section for
/// why those others are a deliberate scope cut.
#[derive(Debug, Clone, Default)]
struct TermEventSink {
    /// Bytes the VT parser generated as a reply owed back to the pty, appended in arrival order.
    pty_writes: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    /// The pending window-title change, if one arrived since [`Self::take_title_update`] last
    /// looked. Two levels of `Option` on purpose, and they mean different things: the outer one
    /// is "did a title event happen at all", the inner one is "what it set the title to" -
    /// `Some(None)` is a real [`AlacEvent::ResetTitle`] clearing the title, which a single
    /// `Option<String>` could not tell apart from "nothing happened".
    title: std::rc::Rc<std::cell::RefCell<Option<Option<String>>>>,
}

impl EventListener for TermEventSink {
    fn send_event(&self, event: AlacEvent) {
        match event {
            AlacEvent::PtyWrite(text) => {
                self.pty_writes
                    .borrow_mut()
                    .extend_from_slice(text.as_bytes());
            }
            AlacEvent::Title(title) => *self.title.borrow_mut() = Some(Some(title)),
            AlacEvent::ResetTitle => *self.title.borrow_mut() = Some(None),
            _ => {}
        }
    }
}

impl TermEventSink {
    /// Takes the pending title change, if any - see [`Self::title`]'s docs for what each layer
    /// of the returned `Option` means.
    fn take_title_update(&self) -> Option<Option<String>> {
        self.title.borrow_mut().take()
    }
}

/// One rendered grid cell: a character plus the styling attributes this pane's renderer draws.
/// Colors are already resolved to concrete RGB triples (see [`resolve_color`]) so the renderer
/// never touches `alacritty_terminal` types directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridCell {
    pub c: char,
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    /// Whether this cell falls inside the live text selection (GitHub issue #158). Derived
    /// every [`TerminalGrid::visible_rows`] call from `alacritty_terminal`'s own
    /// `RenderableContent::selection` (`term/mod.rs:2408`, itself `Term::selection.to_range`),
    /// never stored - so it can't go stale against the real selection the way a separately
    /// tracked copy would.
    pub selected: bool,
    /// How many columns this cell really occupies on screen (GitHub issue #211) - see
    /// [`CellWidth`].
    pub width: CellWidth,
}

/// How many grid columns one [`GridCell`] actually paints into, derived from
/// `alacritty_terminal`'s own `Flags::WIDE_CHAR`/`Flags::WIDE_CHAR_SPACER` (GitHub issue #211 -
/// see the module docs for the exact `Term::input` behaviour this mirrors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellWidth {
    /// An ordinary single-column cell.
    Narrow,
    /// The leading cell of a double-width character (`Flags::WIDE_CHAR`): the glyph itself lives
    /// here and covers this column *and* the next.
    Wide,
    /// The trailing placeholder cell `alacritty_terminal` writes after a wide character
    /// (`Flags::WIDE_CHAR_SPACER`). Its own `c` is a literal `' '` the emulator put there; it is
    /// not a character the program printed, and painting it is exactly the bug this issue fixes.
    Spacer,
}

impl CellWidth {
    /// How many columns' worth of pixels this cell is painted across - see the type's own docs
    /// for why `Spacer` is genuinely zero rather than one.
    pub fn columns(self) -> usize {
        match self {
            CellWidth::Narrow => 1,
            CellWidth::Wide => 2,
            CellWidth::Spacer => 0,
        }
    }
}

/// Which half of a character cell a pointer position fell in - the row/column-space counterpart
/// of `alacritty_terminal::index::Side` (`index.rs:14`, `pub type Side = Direction`), which is
/// what decides whether the cell under the pointer is included in a drag selection or not.
/// Mirrored here rather than re-exporting `Side` so `crate::terminal::pane`'s mouse handling
/// never touches `alacritty_terminal` types directly, matching this module's existing contract
/// for [`GridCell`]'s already-resolved RGB colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellSide {
    Left,
    Right,
}

impl CellSide {
    fn to_alacritty(self) -> Side {
        match self {
            CellSide::Left => Side::Left,
            CellSide::Right => Side::Right,
        }
    }
}

/// A position inside the visible grid, in the same viewport row/column space
/// [`TerminalGrid::visible_rows`] returns (`row` indexes that `Vec`, `column` indexes a row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPosition {
    pub row: usize,
    pub column: usize,
    pub side: CellSide,
}

impl CellPosition {
    /// `display_offset` is `Term`'s own current scroll position (GitHub issue #331 -
    /// [`TerminalGrid::scroll_display`]'s docs), needed here because a viewport row index is
    /// only ever the grid `Line` index at `display_offset == 0`: at any other offset the two
    /// diverge by exactly that many lines, the same shift [`TerminalGrid::visible_rows`] applies
    /// in the opposite direction when reading cells back out. Selecting text while scrolled back
    /// therefore still anchors on the real historical line the pointer is over, not on
    /// whatever's currently live at that same screen row.
    fn to_alacritty(self, display_offset: usize) -> AlacPoint {
        AlacPoint::new(
            Line(self.row as i32 - display_offset as i32),
            Column(self.column),
        )
    }
}

/// How far, and by what unit, to move the viewport into (or back out of) retained scrollback
/// history (GitHub issue #331) - [`TerminalGrid::scroll_display`]'s argument. Mirrors
/// `alacritty_terminal::grid::Scroll` rather than re-exporting it, matching this module's
/// [`CellSide`]/[`CellPosition`] convention of never leaking `alacritty_terminal` types across
/// its own boundary into `crate::terminal::pane`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAmount {
    /// A relative move by `count` grid lines: positive scrolls up into history, negative scrolls
    /// back down toward the live tail. What a mouse-wheel notch or an accumulated trackpad delta
    /// becomes once `crate::terminal::pane` converts pixels to whole lines.
    Lines(i32),
    /// One screenful up into history - a `PageUp` keystroke.
    PageUp,
    /// One screenful back down toward the live tail - a `PageDown` keystroke.
    PageDown,
    /// All the way back to the oldest retained line.
    Top,
    /// All the way back to the live tail - the "jump to bottom" affordance.
    Bottom,
}

/// Every real colour a terminal grid resolves against, already reduced to concrete RGB - the whole
/// interface between the live theme and this pure module (GitHub issue #208).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalPalette {
    /// The pane's own fill, and what `NamedColor::Background` resolves to.
    pub background: (u8, u8, u8),
    /// Unstyled output, and what `NamedColor::Foreground`/`BrightForeground` resolve to.
    pub foreground: (u8, u8, u8),
    /// What `NamedColor::Cursor` resolves to. The block cursor itself is an inverse-video swap
    /// (see [`grid_cell_from_alacritty`]), not a fill painted in this colour.
    pub cursor: (u8, u8, u8),
    /// The fill painted behind a selected cell (GitHub issue #158).
    pub selection: (u8, u8, u8),
    /// The ANSI 16, indexed `0..=15` by `NamedColor`'s own discriminants / `Color::Indexed(0..=15)`.
    pub ansi: [(u8, u8, u8); 16],
}

/// Jerry Dark's own terminal palette, transcribed - the exact literal values
/// `crate::theme::terminal`'s tokens carry as their compiled defaults.
impl Default for TerminalPalette {
    fn default() -> Self {
        Self {
            background: (0x0d, 0x0f, 0x11),
            foreground: (0xa7, 0xad, 0xb4),
            cursor: (0x5a, 0x9a, 0xd4),
            selection: (0x27, 0x3a, 0x4d),
            ansi: [
                (0x00, 0x00, 0x00), // 0 black
                (0xcd, 0x31, 0x31), // 1 red
                (0x0d, 0xbc, 0x79), // 2 green
                (0xe5, 0xe5, 0x10), // 3 yellow
                (0x24, 0x72, 0xc8), // 4 blue
                (0xbc, 0x3f, 0xbc), // 5 magenta
                (0x11, 0xa8, 0xcd), // 6 cyan
                (0xe5, 0xe5, 0xe5), // 7 white
                (0x66, 0x66, 0x66), // 8 bright black
                (0xf1, 0x4c, 0x4c), // 9 bright red
                (0x23, 0xd1, 0x8b), // 10 bright green
                (0xf5, 0xf5, 0x43), // 11 bright yellow
                (0x3b, 0x8e, 0xea), // 12 bright blue
                (0xd6, 0x70, 0xd6), // 13 bright magenta
                (0x29, 0xb8, 0xdb), // 14 bright cyan
                (0xff, 0xff, 0xff), // 15 bright white
            ],
        }
    }
}

/// Halves each channel - used for `NamedColor`'s `Dim*` variants, which have no fixed standard
/// RGB value the way the base 16 colors do.
fn dim(color: (u8, u8, u8)) -> (u8, u8, u8) {
    (color.0 / 2, color.1 / 2, color.2 / 2)
}

fn named_color_rgb(name: NamedColor, palette: &TerminalPalette) -> (u8, u8, u8) {
    match name {
        NamedColor::Black => palette.ansi[0],
        NamedColor::Red => palette.ansi[1],
        NamedColor::Green => palette.ansi[2],
        NamedColor::Yellow => palette.ansi[3],
        NamedColor::Blue => palette.ansi[4],
        NamedColor::Magenta => palette.ansi[5],
        NamedColor::Cyan => palette.ansi[6],
        NamedColor::White => palette.ansi[7],
        NamedColor::BrightBlack => palette.ansi[8],
        NamedColor::BrightRed => palette.ansi[9],
        NamedColor::BrightGreen => palette.ansi[10],
        NamedColor::BrightYellow => palette.ansi[11],
        NamedColor::BrightBlue => palette.ansi[12],
        NamedColor::BrightMagenta => palette.ansi[13],
        NamedColor::BrightCyan => palette.ansi[14],
        NamedColor::BrightWhite => palette.ansi[15],
        NamedColor::Foreground | NamedColor::BrightForeground => palette.foreground,
        NamedColor::Background => palette.background,
        NamedColor::Cursor => palette.cursor,
        NamedColor::DimForeground => dim(palette.foreground),
        NamedColor::DimBlack => dim(palette.ansi[0]),
        NamedColor::DimRed => dim(palette.ansi[1]),
        NamedColor::DimGreen => dim(palette.ansi[2]),
        NamedColor::DimYellow => dim(palette.ansi[3]),
        NamedColor::DimBlue => dim(palette.ansi[4]),
        NamedColor::DimMagenta => dim(palette.ansi[5]),
        NamedColor::DimCyan => dim(palette.ansi[6]),
        NamedColor::DimWhite => dim(palette.ansi[7]),
    }
}

/// The standard xterm 256-color cube (indices `16..=231`) and grayscale ramp (`232..=255`)
/// formulas - matches `vendor/zed/crates/terminal/src/terminal.rs`'s `get_color_at_index`/
/// `rgb_for_index` (a public xterm convention, cited from the same source there).
fn indexed_color_rgb(index: u8, palette: &TerminalPalette) -> (u8, u8, u8) {
    match index {
        0..=15 => palette.ansi[index as usize],
        16..=231 => {
            let i = index - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let scale = |c: u8| if c == 0 { 0 } else { c * 40 + 55 };
            (scale(r), scale(g), scale(b))
        }
        232..=255 => {
            let value = (index - 232) * 10 + 8;
            (value, value, value)
        }
    }
}

fn resolve_color(color: Color, palette: &TerminalPalette) -> (u8, u8, u8) {
    match color {
        Color::Named(name) => named_color_rgb(name, palette),
        Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => indexed_color_rgb(index, palette),
    }
}

fn grid_cell_from_alacritty(
    cell: &AlacCell,
    is_cursor: bool,
    selected: bool,
    palette: &TerminalPalette,
) -> GridCell {
    let mut fg = resolve_color(cell.fg, palette);
    let mut bg = resolve_color(cell.bg, palette);
    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if is_cursor {
        std::mem::swap(&mut fg, &mut bg);
    }
    // GitHub issue #211. `LEADING_WIDE_CHAR_SPACER` is deliberately *not* mapped to
    // [`CellWidth::Spacer`]: `alacritty_terminal` writes that one at the end of a row when a wide
    // character wouldn't fit before the wrap (`term/mod.rs:1109-1114`), and the character itself
    // then lands on the *next* row. So it is a genuine, real blank column standing on its own,
    // not the second half of a glyph rendered elsewhere on the same row - it must keep painting
    // its own single column, or every wrapped row would come out one column short.
    let width = if cell.flags.contains(Flags::WIDE_CHAR) {
        CellWidth::Wide
    } else if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        CellWidth::Spacer
    } else {
        CellWidth::Narrow
    };
    GridCell {
        selected,
        width,
        // An uninitialized-by-any-write cell holds `'\0'` (`Cell::default()` in
        // `term/cell.rs` uses `c: ' '` only as the *constructed* default) - render as a
        // space either way.
        c: if cell.c == '\0' { ' ' } else { cell.c },
        fg,
        bg,
        bold: cell.flags.intersects(Flags::BOLD),
        italic: cell.flags.intersects(Flags::ITALIC),
        underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
        strikethrough: cell.flags.contains(Flags::STRIKEOUT),
    }
}

/// An `alacritty_terminal::Term`-backed grid: bytes from a running child process (via
/// `pty-core`, fed in through [`TerminalGrid::append_bytes`]) are parsed as real ANSI/VT100 and
/// land in a real cursor-addressed grid.
pub struct TerminalGrid {
    term: Term<TermEventSink>,
    processor: Processor<StdSyncHandler>,
    size: GridSize,
    /// The other half of the `Term`'s own [`TermEventSink`] - see that type's docs for why this
    /// is an `Rc`'d clone rather than reading straight off `term`.
    events: TermEventSink,
    /// The window title the child process last set via OSC 0/2, drained out of [`Self::events`]
    /// at the end of every [`Self::append_bytes`] so [`Self::title`] can hand out a plain
    /// borrow instead of forcing every reader through the sink's `RefCell`.
    title: Option<String>,
    /// The tee'd OSC 9 / 9;4 / 777 parser - see [`crate::terminal::osc`]'s module docs for why
    /// this is a second, independent parser over the same bytes rather than an extension of
    /// [`Self::processor`].
    osc: OscWatcher,
    /// `true` once the backing process has exited; the grid stops changing after this but
    /// keeps whatever it last rendered, mirroring step 3's `TerminalBuffer::ended`.
    pub ended: bool,
}

impl TerminalGrid {
    pub fn new(rows: u16, cols: u16) -> Self {
        let size = GridSize {
            rows: rows.max(1) as usize,
            cols: cols.max(1) as usize,
        };
        let events = TermEventSink::default();
        let term = Term::new(Config::default(), &size, events.clone());
        Self {
            term,
            processor: Processor::new(),
            size,
            events,
            title: None,
            osc: OscWatcher::default(),
            ended: false,
        }
    }

    /// Feeds a chunk of raw pty bytes into the VT100 parser (`Processor::advance`), which
    /// drives the real `Term` grid state (cursor movement, SGR colors, screen clears, etc.) and
    /// may also queue bytes into [`Self::events`] for [`Self::take_pending_pty_writes`]
    /// to hand back to the pty, and update [`Self::title`] - see [`TermEventSink`]'s own docs.
    pub fn append_bytes(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
        self.osc.feed(bytes);
        if let Some(update) = self.events.take_title_update() {
            self.title = update;
        }
    }

    /// The window title the child process last set via OSC 0/2, or `None` if it has never set
    /// one or last sent a title reset. See [`TermEventSink`]'s docs; classified into a coarse
    /// status signal by `crate::rail::title_signal`.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Consumes the "an OSC 9 / OSC 777 desktop notification fired" flag - see
    /// [`crate::terminal::osc::OscWatcher::take_attention_ping`] for why it is consumed rather
    /// than latched here.
    pub fn take_attention_ping(&mut self) -> bool {
        self.osc.take_attention_ping()
    }

    /// The most recent OSC 9;4 progress report, if the child process is speaking that protocol.
    pub fn progress(&self) -> Option<Progress> {
        self.osc.progress()
    }

    /// Drains and returns any bytes the VT parser generated in response to a terminal query
    /// (e.g. a cursor position report for `ESC[6n`) during the most recent [`Self::append_bytes`]
    /// call(s) - the caller (`crate::terminal::pane`'s poll loop) is responsible for actually
    /// writing these back to the pty's stdin via `PtySession::write_input`. Empty on every call
    /// that didn't just process a query-generating sequence, which is the overwhelmingly common
    /// case - a plain `Vec` (not an `Option`) so the caller can check emptiness without an extra
    /// match arm.
    pub fn take_pending_pty_writes(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.events.pty_writes.borrow_mut())
    }

    /// Resizes the grid to match a new pty size. Must be called alongside `PtySession::resize`
    /// (see `crate::terminal::pane::maybe_resize_pty`) - the two are separate resizes that need
    /// to stay in sync, or the rendered grid's geometry would diverge from what the child
    /// process believes its terminal size is.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.size = GridSize {
            rows: rows.max(1) as usize,
            cols: cols.max(1) as usize,
        };
        self.term.resize(self.size);
        self.discard_blank_scrollback();
    }

    pub fn mark_ended(&mut self) {
        self.ended = true;
    }

    /// The current `(cols, rows)` this grid's `Term` is sized to - backs
    /// `crate::terminal::pane`'s info footer `148×38`-style display.
    pub fn dimensions(&self) -> (u16, u16) {
        (self.size.cols as u16, self.size.rows as u16)
    }

    /// Erases the visible screen *and* the scrollback `Term` retains internally
    /// (`ClearMode::Saved` - this app never surfaces that scrollback as UI, but `Term` still
    /// holds it, and a real clear should drop it too), then homes the cursor. Implemented as
    /// real ANSI bytes through the same [`Self::append_bytes`] path every other byte goes
    /// through (`\x1b[3J` erase saved lines, `\x1b[2J` erase the visible screen, `\x1b[H` home
    /// the cursor - the same sequence a shell's own `clear`/`tput reset` emits).
    pub fn clear(&mut self) {
        self.append_bytes(b"\x1b[3J\x1b[2J\x1b[H");
    }

    /// A snapshot of the currently visible grid (`rows.len() == screen_lines`, each row has
    /// exactly `columns` cells). The cell at the cursor's current position (if visible) has its
    /// fg/bg swapped, so the renderer doesn't need to separately overlay a cursor glyph, and
    /// every cell inside the live selection is flagged [`GridCell::selected`].
    pub fn visible_rows(&self, palette: &TerminalPalette) -> Vec<Vec<GridCell>> {
        let content = self.term.renderable_content();
        let cursor_point =
            (content.cursor.shape != CursorShape::Hidden).then_some(content.cursor.point);
        let selection = content.selection;
        // GitHub issue #331: `display_iter`'s own `point.line` is viewport-relative, running
        // negative for history rows once `display_offset > 0` - see the module docs' scrollback
        // section for why shifting by `display_offset` here is the one change scrolling needed.
        let display_offset = content.display_offset as i32;

        let mut rows: Vec<Vec<GridCell>> = (0..self.size.rows)
            .map(|_| Vec::with_capacity(self.size.cols))
            .collect();

        for indexed in content.display_iter {
            let line = indexed.point.line.0 + display_offset;
            if line < 0 {
                // Shouldn't happen - `display_iter` never yields more than `screen_lines` rows
                // above the viewport top - but guard defensively rather than panicking on an
                // unexpected negative index.
                continue;
            }
            let Some(row) = rows.get_mut(line as usize) else {
                continue;
            };
            let is_cursor = cursor_point == Some(indexed.point);
            let selected = selection.is_some_and(|range| range.contains(indexed.point));
            row.push(grid_cell_from_alacritty(
                indexed.cell,
                is_cursor,
                selected,
                palette,
            ));
        }

        rows
    }

    // -------------------------------------------------------------- scrollback (issue #331)

    /// Scrolls the viewport into (or back out of) retained scrollback history. A thin wrapper
    /// over `Term::scroll_display` - see the module docs' scrollback section for why no
    /// "don't yank the user back to the bottom while new output arrives" bookkeeping is needed
    /// here: `alacritty_terminal`'s own `Grid::scroll_up` already keeps `display_offset` pinned
    /// to the same historical lines whenever it isn't `0`.
    pub fn scroll_display(&mut self, amount: ScrollAmount) {
        let scroll = match amount {
            ScrollAmount::Lines(count) => AlacScroll::Delta(count),
            ScrollAmount::PageUp => AlacScroll::PageUp,
            ScrollAmount::PageDown => AlacScroll::PageDown,
            ScrollAmount::Top => AlacScroll::Top,
            ScrollAmount::Bottom => AlacScroll::Bottom,
        };
        self.term.scroll_display(scroll);
    }

    /// Scrolls directly to an absolute `display_offset` (clamped to
    /// `0..=Self::scroll_history_len`) - `crate::terminal::pane`'s scrollbar click/drag, which
    /// already computes a target line count from where the pointer landed on the track rather
    /// than a relative delta. Implemented as a `Delta` of the difference from
    /// [`Self::scroll_offset`] so `alacritty_terminal`'s own clamping (`grid/mod.rs:163-172`)
    /// stays the single place that enforces the valid range, rather than a second copy of it
    /// here.
    pub fn set_scroll_offset(&mut self, target: usize) {
        // `target`/`Self::scroll_offset` are both bounded by real scrollback line counts (at
        // most `Config::scrolling_history`, 10000, plus a screenful) - nowhere near overflowing
        // an `i32` delta between them.
        let delta = target as i32 - self.scroll_offset() as i32;
        self.scroll_display(ScrollAmount::Lines(delta));
    }

    /// How many lines back into scrollback the viewport currently is - `0` means live (the
    /// normal, unscrolled state), matching `Term`'s own `Grid::display_offset`. Read live, never
    /// mirrored, so it can't drift from what [`Self::visible_rows`] actually painted.
    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// How many lines of real scrollback are currently retained - `0` on the alt screen (see the
    /// module docs' alt-screen note), otherwise growing up to `Config::scrolling_history`
    /// (10000) as output accumulates. Backs the scrollbar's `max_scroll_offset` and the
    /// "is there anything to scroll to at all" checks.
    pub fn scroll_history_len(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Drops any scrollback accumulated so far without disturbing the live viewport, cursor, or
    /// terminal mode (GitHub issue #362) - `crate::terminal::pane::TerminalPane::
    /// maybe_resize_pty`'s own docs for why: `Self::new` sizes this grid (and the pty) at a
    /// placeholder before the pane has ever measured its real content-box size, so a program
    /// that prints output before the corrective resize-down lands can have that output
    /// legitimately (per `alacritty_terminal`'s own shrink-resize semantics - see
    /// `scrolling_up_reveals_lines_pushed_into_history`'s own module docs) evicted into real
    /// scrollback the moment the correction happens, even though the human never saw it
    /// overflow at the size they're actually looking at. Called exactly once, right after the
    /// first real resize that genuinely reached the live pty, to reset the baseline before a
    /// real user-driven resize (window resize, font-size change) can ever legitimately create
    /// scrollback of its own.
    pub fn discard_scrollback(&mut self) {
        self.retain_history(0);
    }

    /// Drops the run of *entirely blank* lines at the **oldest** end of retained scrollback
    /// (GitHub issue #368), leaving everything from the oldest line that has real content
    /// onward exactly as it was.
    pub fn discard_blank_scrollback(&mut self) {
        let history = self.scroll_history_len();
        let grid = self.term.grid();
        let mut blank = 0usize;
        // Oldest first. `Grid`'s own `Index<Line>` runs negative into history
        // (`grid/mod.rs:453`), with `Line(-1)` the line just above the viewport and
        // `Line(-history)` the oldest retained line.
        for offset in (1..=history).rev() {
            if grid[Line(-(offset as i32))].is_clear() {
                blank += 1;
            } else {
                break;
            }
        }
        if blank == 0 {
            return;
        }
        self.retain_history(history - blank);
    }

    /// Shrinks retained scrollback to at most `keep` lines - dropping the *oldest* lines first,
    /// which is what `Grid::update_history` (`grid/mod.rs:154`) already does - then restores the
    /// real `Config::scrolling_history` capacity so later output accumulates history exactly as
    /// before.
    fn retain_history(&mut self, keep: usize) {
        let real_cap = Config::default().scrolling_history;
        let grid = self.term.grid_mut();
        grid.update_history(keep);
        grid.update_history(real_cap);
    }

    /// Everything this terminal has really printed that it still holds - retained scrollback
    /// *and* the visible screen - as trimmed-right text lines, oldest first, capped to the last
    /// `max_lines` of it. GitHub issue #227's real transcript capture: what an agent's own pane
    /// actually said, kept when its run ends so History can show the run's own output rather than
    /// a synthesised stand-in.
    pub fn retained_text_lines(&self, max_lines: usize) -> Vec<String> {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let screen = grid.screen_lines() as i32;
        let mut lines: Vec<String> = Vec::with_capacity((history + screen).max(0) as usize);
        for index in -history..screen {
            let row = &grid[Line(index)];
            let text: String = row
                .into_iter()
                // The spacer's `' '` was written by the emulator, not by the program - keeping it
                // would put a stray space after every wide character, the identical reasoning
                // `crate::terminal::pane::TerminalPane::visible_text_lines` already applies.
                .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
                .map(|cell| cell.c)
                .collect();
            lines.push(text.trim_end().to_string());
        }
        while lines.first().is_some_and(|line| line.is_empty()) {
            lines.remove(0);
        }
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        if lines.len() > max_lines {
            lines.drain(..lines.len() - max_lines);
        }
        lines
    }

    /// `true` once [`Self::scroll_offset`] is anything other than live - i.e. the pane is
    /// genuinely showing scrollback rather than the live tail right now.
    pub fn is_scrolled_back(&self) -> bool {
        self.scroll_offset() > 0
    }

    // ------------------------------------------------------------------ selection (issue #158)

    /// Anchors a new selection at `position`, discarding any previous one - what a real
    /// mouse-down inside the grid does. A selection anchored and never dragged is genuinely
    /// *empty* (`Selection::is_empty`, `selection.rs:193`), so [`Self::selected_text`] returns
    /// `None` for it: a plain click therefore clears the selection rather than selecting one
    /// stray character, without this needing a "was it a drag?" flag of its own.
    pub fn start_selection(&mut self, position: CellPosition) {
        let display_offset = self.scroll_offset();
        self.term.selection = Some(Selection::new(
            SelectionType::Simple,
            position.to_alacritty(display_offset),
            position.side.to_alacritty(),
        ));
    }

    /// Moves the *end* of the in-progress selection to `position` (`Selection::update`,
    /// `selection.rs:133`) - what a real mouse-drag does. A no-op when nothing is anchored, so
    /// an ordinary hover can never conjure a selection out of nothing.
    pub fn update_selection(&mut self, position: CellPosition) {
        let display_offset = self.scroll_offset();
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(
                position.to_alacritty(display_offset),
                position.side.to_alacritty(),
            );
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// The real selected text, straight out of `Term::selection_to_string`
    /// (`term/mod.rs:529`) - `None` when nothing is selected, when the anchored selection is
    /// still empty, or when the selected span is entirely blank (a drag across empty screen has
    /// nothing worth putting on the clipboard, and silently replacing the clipboard with `""`
    /// would destroy whatever the user copied last).
    pub fn selected_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    /// Whether the running program has turned on bracketed-paste mode (`DECSET 2004`), which
    /// decides how [`crate::terminal::pane::TerminalPane`] frames pasted bytes - see that
    /// method's own docs. Read live off `Term::mode()` (`term/mod.rs:709`) rather than latched,
    /// since a program can enable and disable it at any point in its own lifetime.
    pub fn bracketed_paste_enabled(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Which mouse-reporting protocol the running program has turned on right now (GitHub issue
    /// #437): `DECSET 1000`/`1002`/`1003` for what to report, `1006`/`1005` for how to frame it.
    /// "Off" is a [`MouseTracking`] variant rather than an `Option` because a caller has to handle
    /// that state either way. Read live, never latched, for the same reason
    /// [`Self::bracketed_paste_enabled`] is.
    pub fn mouse_protocol(&self) -> MouseProtocol {
        let mode = self.term.mode();
        // Most permissive first, so the result stays sane even if two bits are ever set at once.
        let tracking = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseTracking::ClicksAndMotion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseTracking::ClicksAndDrag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseTracking::Clicks
        } else {
            MouseTracking::Off
        };
        let encoding = if mode.contains(TermMode::SGR_MOUSE) {
            MouseEncoding::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            MouseEncoding::Utf8
        } else {
            MouseEncoding::Normal
        };
        MouseProtocol { tracking, encoding }
    }

    /// Whether a mouse-wheel scroll reaching this pane right now should be translated into key
    /// bytes and forwarded to the child process, rather than moving [`Self::scroll_display`]
    /// (GitHub issue #362, filed after issue #331 shipped `scroll_display` itself but never
    /// checked this). *Which* keys is
    /// `crate::terminal::pane::TerminalPane::forward_scroll_as_page_keys`'s decision, not this
    /// one's - see its docs for why PageUp/PageDown rather than the arrow keys #362 first
    /// shipped (GitHub issue #368).
    pub fn alt_scroll_forwarding_active(&self) -> bool {
        let mode = self.term.mode();
        self.alt_screen_active() && mode.contains(TermMode::ALTERNATE_SCROLL)
    }

    /// Whether a full-screen program currently owns this terminal's alt screen (`TermMode::
    /// ALT_SCREEN`, set by `Term::swap_alt` - `vim`, `less`, `htop`, an agent CLI's own UI).
    pub fn alt_screen_active(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }
}

#[cfg(test)]
mod grid_emulation_tests {
    use super::*;

    fn row_text(row: &[GridCell]) -> String {
        row.iter().map(|cell| cell.c).collect::<String>()
    }

    #[test]
    fn plain_text_lands_on_the_first_row() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello");
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(rows.len(), 5);
        assert!(row_text(&rows[0]).starts_with("hello"));
    }

    #[test]
    fn a_cursor_position_query_is_queued_as_a_real_reply_not_dropped() {
        let mut grid = TerminalGrid::new(5, 20);
        assert!(
            grid.take_pending_pty_writes().is_empty(),
            "sanity check: nothing queued before any query was ever sent"
        );

        grid.append_bytes(b"\x1b[3;5H");
        assert!(
            grid.take_pending_pty_writes().is_empty(),
            "sanity check: moving the cursor alone must not itself queue a reply"
        );

        grid.append_bytes(b"\x1b[6n");
        let reply = grid.take_pending_pty_writes();
        assert_eq!(
            reply, b"\x1b[3;5R",
            "a real cursor position report for row 3, col 5 must be queued, matching what \
             alacritty_terminal's own Term::device_status formats - not dropped, and not some \
             other row/col"
        );

        assert!(
            grid.take_pending_pty_writes().is_empty(),
            "the queue must be genuinely drained by the previous take_pending_pty_writes call, \
             not left with the same reply queued forever"
        );
    }

    #[test]
    fn a_real_osc_title_sequence_is_captured_and_prints_nothing() {
        let mut grid = TerminalGrid::new(5, 40);
        assert_eq!(
            grid.title(),
            None,
            "sanity check: no title before any is set"
        );

        grid.append_bytes("\x1b]0;\u{25d0} Claude Code\x07".as_bytes());
        assert_eq!(grid.title(), Some("\u{25d0} Claude Code"));

        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(
            row_text(&rows[0]).trim(),
            "",
            "an OSC title must be consumed by the parser, never painted into the grid"
        );

        grid.append_bytes(b"\x1b]2;second title\x1b\\");
        assert_eq!(grid.title(), Some("second title"));
    }

    #[test]
    fn a_real_title_reset_clears_the_captured_title() {
        let mut grid = TerminalGrid::new(5, 40);
        grid.append_bytes(b"\x1b[22t"); // push the current (absent) title
        grid.append_bytes(b"\x1b]0;working\x07");
        assert_eq!(grid.title(), Some("working"));

        grid.append_bytes(b"\x1b[23t"); // pop it back to "no title"
        assert_eq!(
            grid.title(),
            None,
            "a title reset must clear the stored title, not leave the stale one standing"
        );
    }

    #[test]
    fn osc_9_family_sequences_reach_the_teed_parser_through_append_bytes() {
        let mut grid = TerminalGrid::new(5, 40);
        assert!(!grid.take_attention_ping());
        assert_eq!(grid.progress(), None);

        grid.append_bytes(b"hello\x1b]9;Agent needs you\x07 world\x1b]9;4;1;60\x07");

        assert!(grid.take_attention_ping());
        assert_eq!(
            grid.progress(),
            Some(Progress {
                state: crate::terminal::osc::ProgressState::Normal,
                percent: Some(60)
            })
        );
        // The surrounding plain text still rendered normally: the tee must not consume bytes
        // from, or otherwise disturb, the primary pipeline.
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(row_text(&rows[0]).trim_end(), "hello world");
    }

    #[test]
    fn cursor_positioning_places_text_at_the_addressed_cell() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"\x1b[3;5HX");
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(rows[2][4].c, 'X');
        assert_eq!(rows[2][0].c, ' ');
        assert_eq!(rows[2][3].c, ' ');
    }

    #[test]
    fn redrawing_at_an_earlier_position_overwrites_in_place() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"\x1b[1;1Hfirst");
        grid.append_bytes(b"\x1b[1;1HSECOND");
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert!(row_text(&rows[0]).starts_with("SECOND"));
    }

    /// GitHub issue #208's pure half. Two renders of the *same* grid state under two different
    /// palettes must produce two different sets of colours - which is only true because every
    /// resolution path (unstyled default fg/bg, a named ANSI colour, and `Color::Indexed(0..=15)`)
    /// reads the passed-in palette rather than a module constant.
    #[test]
    fn every_themeable_colour_comes_from_the_supplied_palette_not_a_hardcoded_one() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes(b"P\x1b[31mR\x1b[0m\x1b[38;5;4mB\x1b[0m");

        let mut ansi = TerminalPalette::default().ansi;
        ansi[1] = (0x77, 0x88, 0x99);
        ansi[4] = (0xaa, 0xbb, 0xcc);
        let themed = TerminalPalette {
            foreground: (0x11, 0x22, 0x33),
            background: (0x44, 0x55, 0x66),
            ansi,
            ..Default::default()
        };

        let default_rows = grid.visible_rows(&TerminalPalette::default());
        let themed_rows = grid.visible_rows(&themed);

        assert_eq!(themed_rows[0][0].c, 'P');
        assert_eq!(themed_rows[0][0].fg, (0x11, 0x22, 0x33));
        assert_eq!(themed_rows[0][0].bg, (0x44, 0x55, 0x66));
        assert_eq!(themed_rows[0][1].c, 'R');
        assert_eq!(themed_rows[0][1].fg, (0x77, 0x88, 0x99));
        assert_eq!(themed_rows[0][2].c, 'B');
        assert_eq!(themed_rows[0][2].fg, (0xaa, 0xbb, 0xcc));

        for column in 0..3 {
            assert_ne!(
                default_rows[0][column].fg, themed_rows[0][column].fg,
                "column {column} rendered the same foreground under two different palettes - it \
                 is still resolving against something hardcoded"
            );
        }
    }

    #[test]
    fn a_program_specified_truecolor_is_left_exactly_alone_by_the_palette() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes(b"\x1b[38;2;1;2;3mX");

        let themed = TerminalPalette {
            foreground: (0x11, 0x22, 0x33),
            ansi: [(0x11, 0x22, 0x33); 16],
            ..Default::default()
        };

        assert_eq!(grid.visible_rows(&themed)[0][0].fg, (1, 2, 3));
    }

    #[test]
    fn the_xterm_256_cube_stays_fixed_while_the_first_sixteen_follow_the_palette() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes(b"\x1b[38;5;196mC\x1b[0m");

        let themed = TerminalPalette {
            ansi: [(0x11, 0x22, 0x33); 16],
            ..Default::default()
        };

        assert_eq!(
            grid.visible_rows(&themed)[0][0].fg,
            (0xff, 0x00, 0x00),
            "index 196 is the cube's own pure red, the same in every real terminal"
        );
    }

    #[test]
    fn sgr_bold_sets_the_bold_flag() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes(b"\x1b[1mB");
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert!(rows[0][0].bold);
    }

    /// A resize has to change what the emulator reports *and* what it hands the renderer - a
    /// `dimensions()` that moved without the painted rows following it would silently paint the
    /// old geometry.
    #[test]
    fn a_resize_changes_both_the_reported_dimensions_and_the_visible_rows() {
        let mut grid = TerminalGrid::new(5, 20);
        assert_eq!(grid.dimensions(), (20, 5));

        grid.resize(10, 40);

        assert_eq!(grid.dimensions(), (40, 10));
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0].len(), 40);
    }

    #[test]
    fn clear_erases_visible_text_and_homes_the_cursor() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"\x1b[3;5Hhello");
        assert_eq!(
            row_text(&grid.visible_rows(&TerminalPalette::default())[2]).trim(),
            "hello"
        );

        grid.clear();
        for row in grid.visible_rows(&TerminalPalette::default()) {
            assert_eq!(
                row_text(&row).trim(),
                "",
                "clear must erase every visible row"
            );
        }

        // The cursor is homed to (1,1) - writing right after `clear()` lands at the top-left,
        // not wherever the cursor happened to be before.
        grid.append_bytes(b"X");
        assert_eq!(grid.visible_rows(&TerminalPalette::default())[0][0].c, 'X');
    }

    /// Pushes `count` numbered lines (`"line 0\r\n"`, `"line 1\r\n"`, ...) through a real grid -
    /// the shared fixture the scrollback tests below build on. Each line is short and
    /// distinguishable by its own number, so a test can assert exactly which lines are on
    /// screen at a given scroll position rather than just that *something* rendered.
    fn grid_with_numbered_lines(rows: u16, cols: u16, count: usize) -> TerminalGrid {
        let mut grid = TerminalGrid::new(rows, cols);
        for i in 0..count {
            grid.append_bytes(format!("line {i}\r\n").as_bytes());
        }
        grid
    }

    /// GitHub issue #227's transcript capture, at the grid level: a run's stored transcript must
    /// be the run's *whole* retained output, not the last screenful, and must stop at the cap.
    mod retained_transcript_tests {
        use super::*;

        #[test]
        fn a_transcript_reaches_back_past_the_visible_screen_into_real_scrollback() {
            let grid = grid_with_numbered_lines(5, 20, 30);
            let lines = grid.retained_text_lines(1000);
            assert_eq!(
                lines.len(),
                30,
                "every printed line must survive, not just the visible screenful"
            );
            assert_eq!(lines.first().map(String::as_str), Some("line 0"));
            assert_eq!(lines.last().map(String::as_str), Some("line 29"));
        }

        #[test]
        fn the_cap_keeps_the_end_of_the_run_not_its_beginning() {
            let grid = grid_with_numbered_lines(5, 20, 30);
            let lines = grid.retained_text_lines(4);
            assert_eq!(
                lines,
                vec![
                    "line 26".to_string(),
                    "line 27".to_string(),
                    "line 28".to_string(),
                    "line 29".to_string(),
                ],
                "a capped transcript must end where the run ended"
            );
        }

        #[test]
        fn the_empty_rest_of_the_screen_is_not_part_of_the_transcript() {
            let mut grid = TerminalGrid::new(10, 20);
            grid.append_bytes(b"first\r\nsecond\r\n");
            assert_eq!(
                grid.retained_text_lines(1000),
                vec!["first".to_string(), "second".to_string()]
            );
        }

        #[test]
        fn a_wide_character_contributes_one_char_not_two() {
            let mut grid = TerminalGrid::new(4, 20);
            grid.append_bytes("\u{4f60}\u{597d} ok\r\n".as_bytes());
            assert_eq!(
                grid.retained_text_lines(1000),
                vec!["\u{4f60}\u{597d} ok".to_string()]
            );
        }
    }

    mod scrollback_tests {
        use super::*;

        /// A shell's real startup output: the blank line most prompts print before themselves,
        /// then a long, single-line prompt. Deliberately longer than the narrow widths the
        /// resize tests below use, so `alacritty_terminal`'s reflow really has something to
        /// re-wrap - which is the whole mechanism GitHub issue #368 is about.
        const SHELL_STARTUP: &[u8] =
            b"\r\n/tmp/fix-terminal-scroll-round3 on main! at 0:55:28\r\n$ ";

        #[test]
        fn narrowing_a_just_opened_pane_leaves_it_with_nothing_to_scroll_to() {
            let mut grid = TerminalGrid::new(36, 110);
            grid.append_bytes(SHELL_STARTUP);
            assert_eq!(
                grid.scroll_history_len(),
                0,
                "sanity check: a prompt is nowhere near overflowing a 36-row viewport"
            );

            grid.resize(26, 38);

            assert_eq!(
                grid.scroll_history_len(),
                0,
                "an essentially empty pane must have nothing to scroll to after a resize - the \
                 only thing the reflow pushed into history was a blank line nobody ever watched \
                 scroll past"
            );
            grid.scroll_display(ScrollAmount::Lines(3));
            assert_eq!(
                grid.scroll_offset(),
                0,
                "and the wheel must therefore be genuinely inert, not move the view by a blank \
                 line"
            );
            assert!(!grid.is_scrolled_back());
        }

        #[test]
        fn a_resize_never_leaves_a_blank_line_at_the_top_of_the_scroll_track() {
            for (rows, cols) in [
                (26u16, 38u16),
                (10, 20),
                (14, 38),
                (20, 60),
                (48, 200),
                (5, 12),
            ] {
                let mut grid = TerminalGrid::new(36, 110);
                grid.append_bytes(SHELL_STARTUP);
                grid.resize(rows, cols);
                if grid.scroll_history_len() == 0 {
                    continue; // nothing to scroll to at all, which is the ideal outcome
                }
                grid.scroll_display(ScrollAmount::Top);
                let top = row_text(&grid.visible_rows(&TerminalPalette::default())[0]);
                assert!(
                    !top.trim().is_empty(),
                    "at {rows}x{cols} the oldest retained line is blank ({top:?}) - scrolling up \
                     would move the view and show the human nothing"
                );
            }
        }

        #[test]
        fn a_resize_still_evicts_real_content_into_real_scrollback() {
            let mut grid = grid_with_numbered_lines(10, 40, 9);
            assert_eq!(
                grid.scroll_history_len(),
                0,
                "sanity check: nine lines fit a ten-row screen"
            );

            grid.resize(4, 40);

            assert!(
                grid.scroll_history_len() > 0,
                "shrinking under the real content must still push it into real scrollback"
            );
            grid.scroll_display(ScrollAmount::Top);
            assert_eq!(
                row_text(&grid.visible_rows(&TerminalPalette::default())[0]).trim(),
                "line 0",
                "and the oldest retained line must still be the oldest real line of output"
            );
        }

        #[test]
        fn a_blank_line_inside_real_output_is_never_trimmed_away() {
            let mut grid = TerminalGrid::new(20, 40);
            grid.append_bytes(b"first\r\n");
            for _ in 0..8 {
                grid.append_bytes(b"\r\n");
            }
            grid.append_bytes(b"last\r\n");

            grid.resize(3, 40);

            grid.scroll_display(ScrollAmount::Top);
            let rows = grid.visible_rows(&TerminalPalette::default());
            assert_eq!(
                row_text(&rows[0]).trim(),
                "first",
                "the oldest retained line still has real content on it"
            );
            assert_eq!(
                row_text(&rows[1]).trim(),
                "",
                "and the blank line the program itself printed underneath it is still there"
            );
        }

        #[test]
        fn scrolling_up_reveals_lines_pushed_into_history() {
            // A 5-row screen, 30 lines written - the first 25 are pushed into scrollback, and
            // the live viewport shows lines 25..30.
            let mut grid = grid_with_numbered_lines(5, 20, 30);
            assert_eq!(grid.scroll_offset(), 0, "sanity check: starts live");
            assert_eq!(
                row_text(&grid.visible_rows(&TerminalPalette::default())[0]).trim(),
                "line 26"
            );

            // One page up (5 rows, matching the screen height) must reveal the five lines
            // immediately above what was on screen.
            grid.scroll_display(ScrollAmount::PageUp);
            assert_eq!(grid.scroll_offset(), 5);
            let rows = grid.visible_rows(&TerminalPalette::default());
            assert_eq!(row_text(&rows[0]).trim(), "line 21");
            assert_eq!(row_text(&rows[4]).trim(), "line 25");
        }

        #[test]
        fn top_and_bottom_reach_the_real_extremes() {
            let mut grid = grid_with_numbered_lines(5, 20, 30);

            grid.scroll_display(ScrollAmount::Top);
            assert_eq!(
                grid.scroll_offset(),
                grid.scroll_history_len(),
                "Top must land exactly on the oldest retained line, not short of it"
            );
            assert_eq!(
                row_text(&grid.visible_rows(&TerminalPalette::default())[0]).trim(),
                "line 0"
            );

            grid.scroll_display(ScrollAmount::Bottom);
            assert_eq!(grid.scroll_offset(), 0);
            assert!(!grid.is_scrolled_back());
            assert_eq!(
                row_text(&grid.visible_rows(&TerminalPalette::default())[0]).trim(),
                "line 26"
            );
        }

        #[test]
        fn new_output_while_scrolled_back_does_not_move_the_viewport() {
            let mut grid = grid_with_numbered_lines(5, 20, 30);
            grid.scroll_display(ScrollAmount::PageUp); // offset 5, viewing lines 20..25
            let before = grid.visible_rows(&TerminalPalette::default());
            assert_eq!(row_text(&before[0]).trim(), "line 21");

            // Ten more lines of real output arrive - as if the shell kept printing while the
            // human was scrolled back reading history.
            for i in 30..40 {
                grid.append_bytes(format!("line {i}\r\n").as_bytes());
            }

            let after = grid.visible_rows(&TerminalPalette::default());
            assert_eq!(
                row_text(&after[0]).trim(),
                "line 21",
                "new output while scrolled back must not move the viewport off the line the \
                 user was looking at"
            );
            assert_eq!(row_text(&after[4]).trim(), "line 25");
            assert!(
                grid.is_scrolled_back(),
                "must still genuinely be scrolled back, not silently reset to live"
            );
            assert!(
                grid.scroll_history_len() > 30,
                "the ten new lines must still have landed in real retained history underneath, \
                 not been dropped: {}",
                grid.scroll_history_len()
            );
        }

        #[test]
        fn set_scroll_offset_jumps_directly_to_an_absolute_target() {
            let mut grid = grid_with_numbered_lines(5, 20, 30);
            let history_len = grid.scroll_history_len();

            grid.set_scroll_offset(history_len);
            assert_eq!(grid.scroll_offset(), history_len);
            assert_eq!(
                row_text(&grid.visible_rows(&TerminalPalette::default())[0]).trim(),
                "line 0"
            );

            grid.set_scroll_offset(0);
            assert_eq!(grid.scroll_offset(), 0);
            assert_eq!(
                row_text(&grid.visible_rows(&TerminalPalette::default())[0]).trim(),
                "line 26"
            );
        }

        #[test]
        fn scroll_amount_lines_clamps_at_both_ends() {
            let mut grid = grid_with_numbered_lines(5, 20, 30);
            let history_len = grid.scroll_history_len();

            grid.scroll_display(ScrollAmount::Lines(history_len as i32 + 1_000));
            assert_eq!(grid.scroll_offset(), history_len);

            grid.scroll_display(ScrollAmount::Lines(-1_000_000));
            assert_eq!(grid.scroll_offset(), 0);
        }

        #[test]
        fn a_selection_anchored_while_scrolled_back_selects_the_real_historical_line() {
            let mut grid = grid_with_numbered_lines(5, 20, 30);
            grid.scroll_display(ScrollAmount::PageUp); // offset 5, viewing lines 20..25
            assert_eq!(
                row_text(&grid.visible_rows(&TerminalPalette::default())[0]).trim(),
                "line 21"
            );

            grid.start_selection(CellPosition {
                row: 0,
                column: 0,
                side: CellSide::Left,
            });
            grid.update_selection(CellPosition {
                row: 0,
                column: 6,
                side: CellSide::Right,
            });

            assert_eq!(
                grid.selected_text().as_deref().map(str::trim),
                Some("line 21"),
                "the selection must follow the real scrolled-to line, not whatever is live at \
                 viewport row 0 right now (which is \"line 25\")"
            );
        }

        #[test]
        fn the_alt_screen_reports_no_scrollback_and_a_scroll_attempt_is_a_real_no_op() {
            let mut grid = grid_with_numbered_lines(5, 20, 30);
            assert!(
                grid.scroll_history_len() > 0,
                "sanity check: real history exists first"
            );

            grid.append_bytes(b"\x1b[?1049h"); // enter the alt screen
            assert_eq!(
                grid.scroll_history_len(),
                0,
                "the alt screen's own grid must report zero scrollback"
            );

            grid.scroll_display(ScrollAmount::PageUp);
            assert_eq!(
                grid.scroll_offset(),
                0,
                "a scroll attempt on the alt screen must not move it into a history it doesn't have"
            );
        }

        #[test]
        fn alt_scroll_forwarding_tracks_the_real_alt_screen_state() {
            let mut grid = TerminalGrid::new(5, 20);
            assert!(
                !grid.alt_scroll_forwarding_active(),
                "the normal screen must never forward scroll as key presses"
            );

            grid.append_bytes(b"\x1b[?1049h"); // enter the alt screen
            assert!(
                grid.alt_scroll_forwarding_active(),
                "the alt screen, with ALTERNATE_SCROLL on by default, must forward"
            );

            grid.append_bytes(b"\x1b[?1049l"); // exit the alt screen
            assert!(
                !grid.alt_scroll_forwarding_active(),
                "leaving the alt screen must hand control back to real scroll_display"
            );
        }

        #[test]
        fn alt_scroll_forwarding_respects_the_real_alternate_scroll_opt_out() {
            let mut grid = TerminalGrid::new(5, 20);
            grid.append_bytes(b"\x1b[?1049h"); // enter the alt screen
            assert!(
                grid.alt_scroll_forwarding_active(),
                "sanity check: on by default"
            );

            grid.append_bytes(b"\x1b[?1007l"); // DECRST 1007: opt out of alternate scroll
            assert!(
                !grid.alt_scroll_forwarding_active(),
                "a program that explicitly disables DECSET 1007 must not have scroll forwarded \
                 to it as synthetic input"
            );
        }
    }

    #[test]
    fn end_to_end_real_pty_cursor_positioning_lands_correctly() {
        let session = pty_core::spawn(pty_core::SpawnOptions::new("printf").arg("\\033[2;3HOK"))
            .expect("spawning `printf` should succeed - this environment must have printf on PATH");

        let mut grid = TerminalGrid::new(5, 20);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while let Ok(chunk) = session
            .output()
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            grid.append_bytes(&chunk);
        }

        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(
            rows[1][2].c, 'O',
            "expected 'O' at row 2 col 3, got rows: {rows:?}"
        );
        assert_eq!(rows[1][3].c, 'K');
    }
}

/// GitHub issue #211: `Flags::WIDE_CHAR`/`WIDE_CHAR_SPACER` were never read here, so the renderer
/// had no way to tell a double-width glyph from an ordinary one - see the module docs. These drive
/// real UTF-8 bytes (CJK, an emoji, a wrapped-at-the-edge wide character) through the same real
/// VT parser everything else goes through and assert on what [`GridCell::width`] genuinely reports.
#[cfg(test)]
mod wide_char_tests {
    use super::*;

    fn widths(row: &[GridCell]) -> Vec<CellWidth> {
        row.iter().map(|cell| cell.width).collect()
    }

    /// The core fact this issue rests on, straight out of real parsed UTF-8: what counts as a
    /// double-width character and what does not. CJK ideographs and emoji occupy their own cell
    /// plus a following spacer; `é` is multi-*byte* but single-*width*, and conflating the two
    /// would push every accented Latin character a column to the right.
    ///
    /// The spacer's own `c` is checked here too: it is a literal space `alacritty_terminal` wrote
    /// (`term/mod.rs:1127-1129`), not a copy of the character or a NUL - which is precisely why
    /// painting it used to insert a stray blank column after every wide glyph. Pinned so a future
    /// dependency bump that changed it can't silently invalidate the renderer's assumption.
    #[test]
    fn real_utf8_is_labelled_wide_or_narrow_by_its_real_column_width() {
        let wide = || vec![CellWidth::Wide, CellWidth::Spacer];
        for (text, chars, expected) in [
            (
                "你好X",
                vec!['你', '好', 'X'],
                [wide(), wide(), vec![CellWidth::Narrow]].concat(),
            ),
            // `🎉` (U+1F389) is a real 4-byte sequence `unicode-width` reports as two columns.
            (
                "🎉ok",
                vec!['🎉', 'o', 'k'],
                [wide(), vec![CellWidth::Narrow; 2]].concat(),
            ),
            ("éa", vec!['é', 'a'], vec![CellWidth::Narrow; 2]),
        ] {
            let mut grid = TerminalGrid::new(2, 10);
            grid.append_bytes(text.as_bytes());
            let rows = grid.visible_rows(&TerminalPalette::default());

            assert_eq!(
                widths(&rows[0][..expected.len()]),
                expected,
                "cell widths for {text:?}"
            );
            let painted: Vec<char> = rows[0][..expected.len()]
                .iter()
                .zip(&expected)
                .filter(|(_, width)| **width != CellWidth::Spacer)
                .map(|(cell, _)| cell.c)
                .collect();
            assert_eq!(painted, chars, "characters for {text:?}");
            for (cell, width) in rows[0].iter().zip(&expected) {
                if *width == CellWidth::Spacer {
                    assert_eq!(cell.c, ' ', "a spacer cell holds a plain space of its own");
                }
            }
        }
    }

    #[test]
    fn a_wide_character_wrapped_off_the_row_end_leaves_a_real_blank_column_not_a_spacer() {
        let mut grid = TerminalGrid::new(3, 5);
        grid.append_bytes("abcd好".as_bytes());
        let rows = grid.visible_rows(&TerminalPalette::default());

        assert_eq!(rows[0][4].c, ' ');
        assert_eq!(
            rows[0][4].width,
            CellWidth::Narrow,
            "the wrap placeholder is its own real column, not the second half of a glyph painted \
             on this row"
        );
        assert_eq!(rows[1][0].c, '好');
        assert_eq!(rows[1][0].width, CellWidth::Wide);
        assert_eq!(rows[1][1].width, CellWidth::Spacer);
    }

    #[test]
    fn end_to_end_real_pty_utf8_output_lands_as_wide_cells() {
        let session = pty_core::spawn(pty_core::SpawnOptions::new("printf").arg("日本語 🎉 ok"))
            .expect("spawning `printf` should succeed - this environment must have printf on PATH");

        let mut grid = TerminalGrid::new(5, 40);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while let Ok(chunk) = session
            .output()
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            grid.append_bytes(&chunk);
        }

        let rows = grid.visible_rows(&TerminalPalette::default());
        let painted: String = rows[0]
            .iter()
            .filter(|cell| cell.width != CellWidth::Spacer)
            .map(|cell| cell.c)
            .collect();
        assert_eq!(
            painted.trim_end(),
            "日本語 🎉 ok",
            "the real pty's UTF-8 must survive as the exact characters printed, once each; got \
             rows: {rows:?}"
        );
        assert_eq!(
            widths(&rows[0][..8]),
            vec![
                CellWidth::Wide, // 日
                CellWidth::Spacer,
                CellWidth::Wide, // 本
                CellWidth::Spacer,
                CellWidth::Wide, // 語
                CellWidth::Spacer,
                CellWidth::Narrow, // space
                CellWidth::Wide,   // 🎉
            ],
        );
    }
}

/// GitHub issue #158: before this, `TerminalGrid` exposed no selection surface at all -
/// `Term::selection` was never written and `Term::selection_to_string` never called, so there
/// was nothing for a Copy binding to copy even once one existed. These cover the real
/// `alacritty_terminal` selection API this module now drives, with no GPUI window involved.
#[cfg(test)]
mod selection_tests {
    use super::*;

    fn left(row: usize, column: usize) -> CellPosition {
        CellPosition {
            row,
            column,
            side: CellSide::Left,
        }
    }

    fn right(row: usize, column: usize) -> CellPosition {
        CellPosition {
            row,
            column,
            side: CellSide::Right,
        }
    }

    /// Every way a gesture can end up selecting nothing worth copying - none of which may
    /// overwrite the clipboard with an empty string or a stray character.
    #[test]
    fn nothing_worth_copying_is_never_reported_as_a_selection() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello world");
        assert_eq!(grid.selected_text(), None, "no selection has been anchored");

        // A plain click (anchor, no drag) is genuinely empty - it must clear a selection, not
        // select the one character under the pointer.
        grid.start_selection(left(0, 3));
        assert_eq!(
            grid.selected_text(),
            None,
            "an anchored-but-never-dragged selection must not put a stray character on the \
             clipboard"
        );

        // A real drag, but across rows that have nothing on them at all.
        grid.start_selection(left(2, 0));
        grid.update_selection(right(2, 15));
        assert_eq!(
            grid.selected_text(),
            None,
            "an all-blank span must not overwrite the clipboard with an empty string"
        );
    }

    #[test]
    fn a_drag_across_one_row_selects_exactly_those_characters() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello world");

        // Columns 6..=10 are "world"; anchoring on the left of column 6 and releasing on the
        // right of column 10 is exactly the span a real left-to-right drag produces.
        grid.start_selection(left(0, 6));
        grid.update_selection(right(0, 10));

        assert_eq!(grid.selected_text().as_deref(), Some("world"));
    }

    #[test]
    fn a_backwards_drag_selects_the_same_span_as_a_forwards_one() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello world");

        grid.start_selection(right(0, 10));
        grid.update_selection(left(0, 6));

        assert_eq!(
            grid.selected_text().as_deref(),
            Some("world"),
            "`Selection::to_range` orders its own anchors, so dragging right-to-left must \
             produce the identical text"
        );
    }

    #[test]
    fn a_multi_row_drag_selects_across_the_line_break() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"first line\r\nsecond line");

        grid.start_selection(left(0, 6));
        grid.update_selection(right(1, 5));

        assert_eq!(
            grid.selected_text().as_deref(),
            Some("line\nsecond"),
            "a real cross-row selection keeps the line break `Term::bounds_to_string` inserts"
        );
    }

    #[test]
    fn clearing_the_selection_really_clears_it() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello world");
        grid.start_selection(left(0, 0));
        grid.update_selection(right(0, 4));
        assert_eq!(grid.selected_text().as_deref(), Some("hello"));

        grid.clear_selection();
        assert_eq!(grid.selected_text(), None);
    }

    #[test]
    fn clearing_the_screen_drops_the_selection_without_this_module_doing_anything() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello world");
        grid.start_selection(left(0, 0));
        grid.update_selection(right(0, 4));
        assert_eq!(grid.selected_text().as_deref(), Some("hello"));

        grid.clear();
        assert_eq!(grid.selected_text(), None);
    }

    /// The selection has to be *visible*, not just readable - a `GridCell` that never carried
    /// the flag would leave the renderer with nothing to paint, so a user dragging across the
    /// terminal would see no feedback at all.
    #[test]
    fn selected_cells_are_flagged_for_the_renderer() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello world");
        assert!(
            grid.visible_rows(&TerminalPalette::default())
                .iter()
                .flatten()
                .all(|cell| !cell.selected),
            "sanity check: with no selection anchored, no cell carries the flag"
        );

        grid.start_selection(left(0, 6));
        grid.update_selection(right(0, 10));

        let rows = grid.visible_rows(&TerminalPalette::default());
        let flagged: String = rows[0]
            .iter()
            .filter(|cell| cell.selected)
            .map(|cell| cell.c)
            .collect();
        assert_eq!(flagged, "world");
        assert!(
            rows[1].iter().all(|cell| !cell.selected),
            "a single-row selection must not flag cells on any other row"
        );
    }

    /// A wide character's *trailing spacer* column is genuinely part of the selection as far as
    /// `alacritty_terminal` is concerned, but the copied text must still contain the character
    /// exactly once - `Term::line_to_string` skips spacer cells (`term/mod.rs:605`). Proves
    /// [`GridCell::width`] didn't have to teach the clipboard anything: dragging over the second
    /// half of `好` copies `好`, not `好 `.
    #[test]
    fn selecting_a_wide_character_copies_it_once_not_once_plus_its_spacer() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes("你好".as_bytes());

        grid.start_selection(left(0, 0));
        grid.update_selection(right(0, 3));

        assert_eq!(grid.selected_text().as_deref(), Some("你好"));
    }

    #[test]
    fn bracketed_paste_mode_tracks_the_real_terminal_mode() {
        let mut grid = TerminalGrid::new(5, 20);
        assert!(
            !grid.bracketed_paste_enabled(),
            "bracketed paste is off until a program actually enables it"
        );

        grid.append_bytes(b"\x1b[?2004h");
        assert!(grid.bracketed_paste_enabled());

        grid.append_bytes(b"\x1b[?2004l");
        assert!(!grid.bracketed_paste_enabled());
    }
}

#[cfg(test)]
mod mouse_protocol_tests {
    use crate::terminal::grid::TerminalGrid;
    use crate::terminal::mouse::{MouseEncoding, MouseTracking};

    fn grid_after(bytes: &[u8]) -> TerminalGrid {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(bytes);
        grid
    }

    #[test]
    fn mouse_reporting_is_off_until_a_program_asks_for_it() {
        let protocol = TerminalGrid::new(5, 20).mouse_protocol();
        assert_eq!(protocol.tracking, MouseTracking::Off);
        assert_eq!(
            protocol.encoding,
            MouseEncoding::Normal,
            "the original framing is the one a program gets without asking"
        );
    }

    #[test]
    fn decset_1000_turns_on_click_tracking() {
        assert_eq!(
            grid_after(b"\x1b[?1000h").mouse_protocol().tracking,
            MouseTracking::Clicks
        );
    }

    #[test]
    fn decset_1002_reports_drag_and_replaces_click_tracking() {
        assert_eq!(
            grid_after(b"\x1b[?1000h\x1b[?1002h")
                .mouse_protocol()
                .tracking,
            MouseTracking::ClicksAndDrag,
            "the three tracking levels are mutually exclusive, not additive"
        );
    }

    #[test]
    fn decset_1003_reports_all_motion() {
        assert_eq!(
            grid_after(b"\x1b[?1003h").mouse_protocol().tracking,
            MouseTracking::ClicksAndMotion
        );
    }

    #[test]
    fn decset_1006_switches_the_encoding_without_changing_what_is_tracked() {
        let protocol = grid_after(b"\x1b[?1002h\x1b[?1006h").mouse_protocol();
        assert_eq!(protocol.tracking, MouseTracking::ClicksAndDrag);
        assert_eq!(protocol.encoding, MouseEncoding::Sgr);
    }

    #[test]
    fn decset_1005_is_reported_as_the_utf8_encoding_until_1006_supersedes_it() {
        assert_eq!(
            grid_after(b"\x1b[?1005h").mouse_protocol().encoding,
            MouseEncoding::Utf8
        );
        assert_eq!(
            grid_after(b"\x1b[?1005h\x1b[?1006h")
                .mouse_protocol()
                .encoding,
            MouseEncoding::Sgr,
            "the two encodings are mutually exclusive too"
        );
    }

    #[test]
    fn decrst_1000_turns_reporting_back_off() {
        assert_eq!(
            grid_after(b"\x1b[?1000h\x1b[?1000l")
                .mouse_protocol()
                .tracking,
            MouseTracking::Off
        );
    }

    #[test]
    fn turning_off_a_tracking_level_that_was_never_on_leaves_the_live_one_alone() {
        assert_eq!(
            grid_after(b"\x1b[?1002h\x1b[?1000l")
                .mouse_protocol()
                .tracking,
            MouseTracking::ClicksAndDrag,
            "DECRST clears only its own bit - a program disabling 1000 it never set keeps 1002"
        );
    }
}
