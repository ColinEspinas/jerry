//! ANSI/VT100 terminal grid emulation via `alacritty_terminal::Term`.
//!
//! ## Why this replaced `ansi.rs`'s hand-rolled scanner
//!
//! Step 3's `ansi::TerminalBuffer` recognized and dropped CSI/OSC escape sequences rather than
//! interpreting them, so it had no notion of cursor position: a full-screen, cursor-addressed
//! program (`vim`, `htop`, or an interactive agent CLI that redraws its UI in place) would
//! render as a garbled stream of raw draw commands instead of a clean, in-place-updating
//! screen. This module drives `alacritty_terminal::Term` - the same crate/rev `vendor/zed`'s
//! own `terminal` crate uses (`vendor/zed/Cargo.toml`, pinned identically here) - instead of a
//! plain-text scan.
//!
//! ## API surface, verified against the pinned rev's real source
//!
//! Every signature below was checked against the fetched dependency source at
//! `~/.cargo/git/checkouts/alacritty-*/*/alacritty_terminal/src/`, since Zed's own
//! `vendor/zed/crates/terminal/src/alacritty.rs` wraps these through Zed-specific types:
//! - `Term::new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T>`
//!   (`term/mod.rs:410`) and `Term::resize<S: Dimensions>(&mut self, size: S)`
//!   (`term/mod.rs:655`). `Dimensions` (`grid/mod.rs:486`) needs only `total_lines`/
//!   `screen_lines`/`columns` - see [`GridSize`].
//! - `Term<T>` only requires `T: EventListener` for `renderable_content`/the `Handler` impl.
//!   This pane polls the grid directly every tick (see `crate::terminal::pane`'s poll loop)
//!   rather than reacting to most events (title changes, bell, clipboard, ...), which
//!   [`PtyWriteQueue`] does still drop. `Event::PtyWrite` is the one real exception - see that
//!   type's own docs for why dropping it is a genuine correctness bug, not a scope cut.
//! - Feeding bytes: `Processor::<StdSyncHandler>::advance(&mut self, handler: &mut H, bytes:
//!   &[u8]) where H: Handler` (`vte-0.15.0/src/ansi.rs:298`); `Term<T: EventListener>`
//!   implements `Handler`. `alacritty_terminal` re-exports its exact `vte` dependency as
//!   `alacritty_terminal::vte`, so this crate depends on `alacritty_terminal` alone rather than
//!   also pinning a separate top-level `vte` that could drift out of lockstep with it.
//! - Reading the grid: `Term::renderable_content(&self) -> RenderableContent<'_>` whose
//!   `display_iter` yields `Indexed<&Cell> { point: Point, cell: &Cell }` for exactly the
//!   visible viewport (at `display_offset == 0`: `point.line` ranges `0..screen_lines`,
//!   `point.column` ranges `0..columns`) - not the whole scrollback. `Cell`'s real fields are
//!   `c: char, fg: Color, bg: Color, flags: Flags, extra: ...` (`term/cell.rs:134`).
//!
//! ## Scope cut: no scrollback UI
//!
//! `Term` retains scrollback history (`Config::scrolling_history`, default 10000 lines), but
//! `display_iter` at `display_offset == 0` only ever exposes the live viewport -
//! `Term::scroll_display` (mouse-wheel/PageUp scrolling into history) isn't wired up here. Not
//! an oversight - a natural following step.
//!
//! ## Scope cut: no OSC 4/10/11 customization
//!
//! `Term::colors` (a palette OSC 4/10/11 sequences can override) starts out entirely `None` and
//! this module never populates or consults it. A program that repalettes its own colors via OSC
//! renders with the theme's palette instead.
//!
//! ## The palette is the caller's, not this module's (GitHub issue #208)
//!
//! Named/indexed colors resolve against a [`TerminalPalette`] the caller passes into
//! [`TerminalGrid::visible_rows`], plus the standard xterm 256-color cube/grayscale formulas for
//! `Color::Indexed(16..=255)` (matching `vendor/zed/crates/terminal/src/terminal.rs`'s
//! `get_color_at_index`/`rgb_for_index`, a public xterm convention).
//!
//! Those sixteen-plus-four colors used to be hardcoded module constants, which is why the terminal
//! rendered as one fixed set of VS Code default values regardless of which of this app's six themes
//! was selected. They are now real, registered `crate::theme::terminal` tokens - but this module
//! deliberately does not read them itself. It stays entirely free of `gpui::Window`/theme access
//! (the same pure-module discipline `crate::code_surface::fold` keeps, and for the same reason:
//! grid state and color resolution have to stay unit-testable with no real GPUI window), so
//! resolving the live theme is `crate::terminal::pane`'s job - it has the theme at paint time -
//! and this module only consumes the already-resolved RGB it hands over.
//!
//! ## Double-width characters (GitHub issue #211)
//!
//! `alacritty_terminal` already tracks, per cell, whether a character is double-width - a CJK
//! ideograph, most emoji, anything `unicode-width` reports as two columns. Verified against the
//! pinned rev's real source (`term/mod.rs:1103-1130`, `Term::input`): a wide character is written
//! into its own cell with `Flags::WIDE_CHAR` set, and the *next* cell is then written with a
//! literal `' '` and `Flags::WIDE_CHAR_SPACER` set. That spacer is a placeholder holding the
//! second column the glyph occupies, not a character of its own - `Term::line_to_string`
//! (`term/mod.rs:605`) skips it when building selection/clipboard text, and no real terminal
//! paints it.
//!
//! Neither flag used to be read here at all, so [`GridCell`] had no way to tell the renderer any
//! of that apart: a row containing `你好` reached `crate::terminal::pane` as the four cells
//! `['你', ' ', '好', ' ']`, which it painted as four glyph advances of ordinary monospace text -
//! the two wide glyphs each taking roughly two advances of their own, plus two stray spaces, so
//! everything after them on the row sat two columns too far right. [`CellWidth`] is what closes
//! that: see its own docs, and `crate::terminal::pane::row_runs` for what the renderer does with
//! it.
//!
//! Not covered: zero-width characters (combining marks, variation selectors, ZWJ) that
//! `alacritty_terminal` stacks onto a cell's `Cell::zerowidth()` side-channel rather than into
//! `Cell::c`. [`GridCell`] still carries exactly one `char` per cell, so those are dropped -
//! `e` + U+0301 renders as a bare `e`, and `❤` + U+FE0F renders in its text presentation. That is
//! unchanged from before this issue, and picking it up means letting one cell contribute more than
//! one character to a row's text, which the link scanner's char-offset-per-cell contract
//! (`crate::terminal::links::LinkMatch`) is currently built on - a real follow-up, not a one-line
//! addition.
//!
//! ## Text selection (GitHub issue #158)
//!
//! Selection is not tracked here at all - it lives in `alacritty_terminal`'s own
//! `Term::selection` field (`term/mod.rs:275`, a real `pub Option<Selection>`), driven through
//! its own `Selection::new`/`Selection::update` API (`selection.rs:125`/`:133`) and read back
//! through `Term::selection_to_string` (`term/mod.rs:529`). This module only translates between
//! that API's `Point`/`Side` coordinates and the row/column [`CellPosition`] the pane's mouse
//! handling produces, so no second, drifting copy of "what is selected" exists. Selection
//! invalidation is likewise `Term`'s own and not re-implemented here: it rotates the selection
//! with the grid when the screen scrolls (`Term::scroll_down_relative`/`scroll_up_relative`'s
//! own `Selection::rotate` calls) and drops it on `ClearMode::All` (`term/mod.rs:1803`) and on
//! an alt-screen swap (`Term::swap_alt`, `:733`) - which is why [`TerminalGrid::clear`], itself
//! just an `ESC[2J` through the same parser, needs no selection handling of its own.

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
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

/// Captures [`AlacEvent::PtyWrite`] and drops every other [`AlacEvent`] (title changes, bell,
/// clipboard, ...) - see the module docs' API surface section for why those others are a
/// deliberate scope cut.
///
/// `PtyWrite` is different: `alacritty_terminal`'s own `Handler` impl for `Term` emits it
/// whenever the VT parser sees a query the *terminal* (not the running program) is supposed to
/// answer back into the pty's stdin - e.g. `ESC[6n` (Device Status Report / cursor position
/// report), already correctly formatted as `ESC[<row>;<col>R` by `Term::device_status`
/// (`term/mod.rs:1332`, verified against the pinned rev's real source per this module's own API
/// surface docs). Dropping it (this type's predecessor, `NoopEventListener`, did) isn't just a
/// missed nicety: on real Windows hardware, ConPTY sends exactly this query as part of its own
/// startup handshake and blocks its *entire* output stream - the child process's own banner,
/// prompt, everything - until it receives a real answer. A terminal that never answers doesn't
/// just render `ESC[6n` oddly, it hangs the pty forever after that first, tiny query - confirmed
/// live: a real Windows build spawned a real `cmd.exe`, the child process itself stayed alive
/// and idle at its own prompt, and `pty-core`'s reader thread read exactly the 4-byte query and
/// then never read another byte, because ConPTY was sitting there waiting on the CPR reply this
/// listener used to throw away.
///
/// `Rc<RefCell<..>>`, not a channel: [`EventListener::send_event`] takes `&self`
/// (`Term<T>` only ever holds a `T`, no `&mut` access once constructed), and this needs to be
/// cheaply `Clone`d so [`TerminalGrid`] can hand `Term::new` its own copy while keeping one to
/// drain from - a `Rc`'d interior-mutable buffer is the direct way to satisfy both without a
/// background thread/channel this is far too small to warrant.
#[derive(Debug, Clone, Default)]
struct PtyWriteQueue(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl EventListener for PtyWriteQueue {
    fn send_event(&self, event: AlacEvent) {
        if let AlacEvent::PtyWrite(text) = event {
            self.0.borrow_mut().extend_from_slice(text.as_bytes());
        }
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
///
/// The three variants' [`CellWidth::columns`] deliberately sum back to the grid's own column
/// count: a `Wide` cell paints the two columns it owns and its following `Spacer` paints none, so
/// the painted x offset of grid column `k` stays exactly `k * cell_width` no matter how many wide
/// characters precede it. That is what keeps `crate::terminal::pane`'s pixel-to-column mouse
/// arithmetic correct on a row full of CJK without it needing to know the row's contents at all -
/// pinned by `crate::terminal::pane::wide_char_render_tests::
/// painted_columns_always_sum_to_the_grid_column_count`.
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
    /// Valid only while `display_offset == 0`, which is always the case here - this module
    /// never calls `Term::scroll_display` (see the module docs' "no scrollback UI" scope cut),
    /// so a viewport row index *is* the grid `Line` index.
    fn to_alacritty(self) -> AlacPoint {
        AlacPoint::new(Line(self.row as i32), Column(self.column))
    }
}

/// Every real colour a terminal grid resolves against, already reduced to concrete RGB - the whole
/// interface between the live theme and this pure module (GitHub issue #208).
///
/// Built from `crate::theme::terminal`'s real registered tokens by
/// `crate::terminal::pane::theme_terminal_palette`, which runs at paint time where the live theme
/// is actually reachable. This module never resolves a token itself; see the module docs.
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
///
/// Duplicated here rather than resolved from `crate::theme` so this module stays theme-free (see
/// the module docs), and kept honest about it by
/// `crate::terminal::pane::terminal_theme_tests::the_pure_grid_default_palette_is_exactly_jerry_darks_own`,
/// which resolves the real tokens and asserts they equal this value - so a retuned token can't
/// leave this stale.
///
/// This is what the module's own tests render against, and what a caller that has no theme to offer
/// gets. The app itself always passes the live theme's palette.
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
///
/// Only `0..=15` is themeable: the cube and the ramp are fixed arithmetic on the index itself, the
/// same in every real terminal, and a program asking for `Color::Indexed(196)` is asking for that
/// exact well-known RGB rather than for "the theme's red".
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
    term: Term<PtyWriteQueue>,
    processor: Processor<StdSyncHandler>,
    size: GridSize,
    /// The other half of the `Term`'s own [`PtyWriteQueue`] - see that type's docs for why this
    /// is an `Rc`'d clone rather than reading straight off `term`.
    pending_pty_writes: PtyWriteQueue,
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
        let pending_pty_writes = PtyWriteQueue::default();
        let term = Term::new(Config::default(), &size, pending_pty_writes.clone());
        Self {
            term,
            processor: Processor::new(),
            size,
            pending_pty_writes,
            ended: false,
        }
    }

    /// Feeds a chunk of raw pty bytes into the VT100 parser (`Processor::advance`), which
    /// drives the real `Term` grid state (cursor movement, SGR colors, screen clears, etc.) and
    /// may also queue bytes into [`Self::pending_pty_writes`] for [`Self::take_pending_pty_writes`]
    /// to hand back to the pty - see [`PtyWriteQueue`]'s own docs.
    pub fn append_bytes(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// Drains and returns any bytes the VT parser generated in response to a terminal query
    /// (e.g. a cursor position report for `ESC[6n`) during the most recent [`Self::append_bytes`]
    /// call(s) - the caller (`crate::terminal::pane`'s poll loop) is responsible for actually
    /// writing these back to the pty's stdin via `PtySession::write_input`. Empty on every call
    /// that didn't just process a query-generating sequence, which is the overwhelmingly common
    /// case - a plain `Vec` (not an `Option`) so the caller can check emptiness without an extra
    /// match arm.
    pub fn take_pending_pty_writes(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.pending_pty_writes.0.borrow_mut())
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
    ///
    /// `palette` is what every `NamedColor`/`Color::Indexed(0..=15)` the running program asked for
    /// resolves against (GitHub issue #208) - passed in per call rather than stored, so a theme
    /// switch is picked up by the very next repaint with no invalidation step that could go stale.
    pub fn visible_rows(&self, palette: &TerminalPalette) -> Vec<Vec<GridCell>> {
        let content = self.term.renderable_content();
        let cursor_point =
            (content.cursor.shape != CursorShape::Hidden).then_some(content.cursor.point);
        let selection = content.selection;

        let mut rows: Vec<Vec<GridCell>> = (0..self.size.rows)
            .map(|_| Vec::with_capacity(self.size.cols))
            .collect();

        for indexed in content.display_iter {
            if indexed.point.line.0 < 0 {
                // Shouldn't happen at `display_offset == 0` (see the module docs), but guard
                // defensively rather than panicking on an unexpected negative index.
                continue;
            }
            let Some(row) = rows.get_mut(indexed.point.line.0 as usize) else {
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

    // ------------------------------------------------------------------ selection (issue #158)

    /// Anchors a new selection at `position`, discarding any previous one - what a real
    /// mouse-down inside the grid does. A selection anchored and never dragged is genuinely
    /// *empty* (`Selection::is_empty`, `selection.rs:193`), so [`Self::selected_text`] returns
    /// `None` for it: a plain click therefore clears the selection rather than selecting one
    /// stray character, without this needing a "was it a drag?" flag of its own.
    pub fn start_selection(&mut self, position: CellPosition) {
        self.term.selection = Some(Selection::new(
            SelectionType::Simple,
            position.to_alacritty(),
            position.side.to_alacritty(),
        ));
    }

    /// Moves the *end* of the in-progress selection to `position` (`Selection::update`,
    /// `selection.rs:133`) - what a real mouse-drag does. A no-op when nothing is anchored, so
    /// an ordinary hover can never conjure a selection out of nothing.
    pub fn update_selection(&mut self, position: CellPosition) {
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(position.to_alacritty(), position.side.to_alacritty());
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
}

#[cfg(test)]
mod tests {
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

    /// [`PtyWriteQueue`]'s real regression coverage: a Device Status Report query (`ESC[6n`,
    /// "where's the cursor?") must produce a real, correctly formatted `ESC[<row>;<col>R` reply
    /// queued for the pty - not silently dropped, which is exactly what real Windows ConPTY
    /// hangs on during its own startup handshake (see [`PtyWriteQueue`]'s own docs for the live
    /// Windows repro this fixes). Uses `\x1b[3;5H` first (the same cursor-positioning sequence
    /// [`cursor_positioning_places_text_at_the_addressed_cell`] already proves lands the cursor
    /// correctly) so the expected reply has a real, non-default row/col to check against - a
    /// query answered from the default `1;1` cursor position wouldn't tell a wrong-position bug
    /// apart from a right one.
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

    /// The key proof that this is real cursor-addressed grid emulation rather than a plain-text
    /// scan: a cursor-positioning CSI sequence (`ESC [ row ; col H`) must place text at that
    /// exact cell, not just append it to a growing line.
    #[test]
    fn cursor_positioning_places_text_at_the_addressed_cell() {
        let mut grid = TerminalGrid::new(5, 20);
        // Move to row 3, column 5 (1-indexed, per CSI CUP), then write "X".
        grid.append_bytes(b"\x1b[3;5HX");
        let rows = grid.visible_rows(&TerminalPalette::default());
        // Row index 2 (0-indexed), column index 4.
        assert_eq!(rows[2][4].c, 'X');
        // Everywhere else on that row is still blank.
        assert_eq!(rows[2][0].c, ' ');
        assert_eq!(rows[2][3].c, ' ');
    }

    /// A second CSI-positioned write to an earlier cell must overwrite in place, not append
    /// after the first - "redraw in place" behavior step 3's plain-text scan couldn't represent.
    #[test]
    fn redrawing_at_an_earlier_position_overwrites_in_place() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"\x1b[1;1Hfirst");
        grid.append_bytes(b"\x1b[1;1HSECOND");
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert!(row_text(&rows[0]).starts_with("SECOND"));
    }

    #[test]
    fn sgr_red_foreground_resolves_to_the_named_palette_color() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes(b"\x1b[31mR\x1b[0m");
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(rows[0][0].c, 'R');
        assert_eq!(rows[0][0].fg, TerminalPalette::default().ansi[1]);
    }

    /// GitHub issue #208's pure half. Two renders of the *same* grid state under two different
    /// palettes must produce two different sets of colours - which is only true because every
    /// resolution path (unstyled default fg/bg, a named ANSI colour, and `Color::Indexed(0..=15)`)
    /// reads the passed-in palette rather than a module constant.
    #[test]
    fn every_themeable_colour_comes_from_the_supplied_palette_not_a_hardcoded_one() {
        let mut grid = TerminalGrid::new(2, 10);
        // "P" unstyled, "R" in named red (SGR 31), "B" in indexed blue (SGR 38;5;4).
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

    /// A cell the running program gave a real 24-bit colour (`SGR 38;2;r;g;b`) is *not* themeable -
    /// the program asked for that exact colour, and a terminal that repainted it in a theme colour
    /// would be corrupting output, not theming it.
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

    /// The xterm 256-colour cube and grayscale ramp (`16..=255`) are fixed arithmetic, not palette
    /// entries - see [`indexed_color_rgb`]'s own docs.
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

    #[test]
    fn resize_changes_the_visible_row_and_column_count() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.resize(10, 40);
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0].len(), 40);
    }

    #[test]
    fn clear_screen_erases_previously_written_text() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello");
        grid.append_bytes(b"\x1b[2J\x1b[1;1H");
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(row_text(&rows[0]).trim(), "");
    }

    #[test]
    fn dimensions_reports_the_real_current_cols_and_rows() {
        let mut grid = TerminalGrid::new(5, 20);
        assert_eq!(grid.dimensions(), (20, 5));
        grid.resize(10, 40);
        assert_eq!(grid.dimensions(), (40, 10));
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

    #[test]
    fn mark_ended_sets_the_flag() {
        let mut grid = TerminalGrid::new(2, 10);
        assert!(!grid.ended);
        grid.mark_ended();
        assert!(grid.ended);
    }

    /// End-to-end through a genuinely spawned process on a real pty (via `pty_core::spawn`)
    /// using real cursor-positioning output (`tput cup`), proving grid *addressing*, not just
    /// line commits.
    #[test]
    fn end_to_end_real_pty_cursor_positioning_lands_correctly() {
        // `printf` with a real CUP escape sequence: move to row 2 col 3, print "OK".
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

    /// The core fact this issue rests on, straight out of real parsed UTF-8: a CJK ideograph
    /// occupies its own cell plus a following spacer cell, and both are now labelled as such.
    #[test]
    fn a_cjk_character_is_a_wide_cell_followed_by_a_spacer_cell() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes("你好X".as_bytes());
        let rows = grid.visible_rows(&TerminalPalette::default());

        assert_eq!(rows[0][0].c, '你');
        assert_eq!(rows[0][2].c, '好');
        assert_eq!(rows[0][4].c, 'X');
        assert_eq!(
            widths(&rows[0][..5]),
            vec![
                CellWidth::Wide,
                CellWidth::Spacer,
                CellWidth::Wide,
                CellWidth::Spacer,
                CellWidth::Narrow,
            ],
        );
    }

    /// The spacer's own `c` is a literal space `alacritty_terminal` wrote there
    /// (`term/mod.rs:1127-1129`), not a copy of the character or a NUL - which is precisely why
    /// painting it used to insert a stray blank column after every wide glyph. Pinned so a future
    /// dependency bump that changed it can't silently invalidate the renderer's assumption.
    #[test]
    fn the_spacer_cell_really_holds_a_plain_space_of_its_own() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes("好".as_bytes());
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(rows[0][1].c, ' ');
        assert_eq!(rows[0][1].width, CellWidth::Spacer);
    }

    /// Emoji, not just CJK - the other half of what issue #211 asks for. `🎉` (U+1F389) is a real
    /// 4-byte UTF-8 sequence and `unicode-width` reports it as two columns.
    #[test]
    fn an_emoji_is_a_wide_cell_too() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes("🎉ok".as_bytes());
        let rows = grid.visible_rows(&TerminalPalette::default());

        assert_eq!(rows[0][0].c, '🎉');
        assert_eq!(rows[0][0].width, CellWidth::Wide);
        assert_eq!(rows[0][1].width, CellWidth::Spacer);
        assert_eq!(rows[0][2].c, 'o');
        assert_eq!(rows[0][2].width, CellWidth::Narrow);
    }

    /// A multi-byte UTF-8 character that is *not* double-width (`é`, two bytes, one column) must
    /// stay [`CellWidth::Narrow`] - "multi-byte" and "double-width" are different properties, and
    /// conflating them would push every accented Latin character a column to the right.
    #[test]
    fn a_multi_byte_but_single_width_character_stays_narrow() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes("éa".as_bytes());
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert_eq!(rows[0][0].c, 'é');
        assert_eq!(widths(&rows[0][..2]), vec![CellWidth::Narrow; 2]);
    }

    /// The end-of-row case: a wide character that doesn't fit in the last column wraps, and
    /// `alacritty_terminal` leaves a `LEADING_WIDE_CHAR_SPACER` behind in that last column. That
    /// one is a real, standalone blank column (the character itself is on the *next* row), so it
    /// must stay [`CellWidth::Narrow`] - see [`grid_cell_from_alacritty`]'s comment.
    #[test]
    fn a_wide_character_wrapped_off_the_row_end_leaves_a_real_blank_column_not_a_spacer() {
        let mut grid = TerminalGrid::new(3, 5);
        // Four narrow characters fill columns 0..=3, leaving only column 4 - too narrow for `好`.
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

    /// End-to-end through a genuinely spawned process on a real pty, so the UTF-8 travels as real
    /// bytes over a real file descriptor rather than being handed to the parser in one clean
    /// slice - which is the case that would break if anything on the read path were byte- rather
    /// than stream-oriented (a 3-byte character split across two `read(2)` calls).
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

    #[test]
    fn nothing_is_selected_until_a_selection_is_actually_dragged() {
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

    /// Pins the module docs' claim that selection invalidation is `alacritty_terminal`'s job:
    /// [`TerminalGrid::clear`] is only an `ESC[2J` through the parser and has no selection code
    /// of its own, so a stale selection surviving a clear would be a real bug (copy would then
    /// return text that is no longer on screen).
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

    #[test]
    fn a_drag_across_blank_screen_selects_nothing_worth_copying() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.start_selection(left(2, 0));
        grid.update_selection(right(2, 15));
        assert_eq!(
            grid.selected_text(),
            None,
            "an all-blank span must not overwrite the clipboard with an empty string"
        );
    }

    /// The selection has to be *visible*, not just readable - a `GridCell` that never carried
    /// the flag would leave the renderer with nothing to paint, so a user dragging across the
    /// terminal would see no feedback at all.
    #[test]
    fn selected_cells_are_flagged_for_the_renderer() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello world");
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

    #[test]
    fn no_selection_means_no_cell_is_flagged() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello world");
        let rows = grid.visible_rows(&TerminalPalette::default());
        assert!(rows.iter().flatten().all(|cell| !cell.selected));
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

        // Columns 0..=3 are `[你][spacer][好][spacer]` - a drag across all four.
        grid.start_selection(left(0, 0));
        grid.update_selection(right(0, 3));

        assert_eq!(grid.selected_text().as_deref(), Some("你好"));
    }

    /// Paste framing depends on this being read from the *live* `Term::mode()` - a program that
    /// turns bracketed paste on (`DECSET 2004`) and later off must be tracked both ways.
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
