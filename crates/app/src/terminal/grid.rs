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
//! ## Scope cut: fixed 16-color palette, no OSC 4/10/11 customization
//!
//! `Term::colors` (a palette OSC 4/10/11 sequences can override) starts out entirely `None` and
//! this module never populates or consults it; named/indexed colors resolve against a fixed,
//! hardcoded ANSI-16 palette plus the standard xterm 256-color cube/grayscale formulas for
//! `Color::Indexed` (matching `vendor/zed/crates/terminal/src/terminal.rs`'s
//! `get_color_at_index`/`rgb_for_index`, a public xterm convention). A program that repalettes
//! its own colors via OSC renders with the default palette instead.

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags};
use alacritty_terminal::term::{Config, Term};
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
}

/// Default foreground/background, used both for `NamedColor::Foreground`/`Background` and this
/// pane's own rendering background - matches step 3's `TerminalPane` colors (`rgb(0xd4d4d4)` on
/// `rgb(0x1e1e1e)`).
pub const DEFAULT_FOREGROUND: (u8, u8, u8) = (0xd4, 0xd4, 0xd4);
pub const DEFAULT_BACKGROUND: (u8, u8, u8) = (0x1e, 0x1e, 0x1e);

/// The standard ANSI 16-color palette (VS Code's default terminal theme values), indexed
/// `0..=15` by `NamedColor`'s own discriminants / `Color::Indexed(0..=15)`.
const NAMED_COLOR_PALETTE: [(u8, u8, u8); 16] = [
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
];

/// Halves each channel - used for `NamedColor`'s `Dim*` variants, which have no fixed standard
/// RGB value the way the base 16 colors do.
fn dim(color: (u8, u8, u8)) -> (u8, u8, u8) {
    (color.0 / 2, color.1 / 2, color.2 / 2)
}

fn named_color_rgb(name: NamedColor) -> (u8, u8, u8) {
    match name {
        NamedColor::Black => NAMED_COLOR_PALETTE[0],
        NamedColor::Red => NAMED_COLOR_PALETTE[1],
        NamedColor::Green => NAMED_COLOR_PALETTE[2],
        NamedColor::Yellow => NAMED_COLOR_PALETTE[3],
        NamedColor::Blue => NAMED_COLOR_PALETTE[4],
        NamedColor::Magenta => NAMED_COLOR_PALETTE[5],
        NamedColor::Cyan => NAMED_COLOR_PALETTE[6],
        NamedColor::White => NAMED_COLOR_PALETTE[7],
        NamedColor::BrightBlack => NAMED_COLOR_PALETTE[8],
        NamedColor::BrightRed => NAMED_COLOR_PALETTE[9],
        NamedColor::BrightGreen => NAMED_COLOR_PALETTE[10],
        NamedColor::BrightYellow => NAMED_COLOR_PALETTE[11],
        NamedColor::BrightBlue => NAMED_COLOR_PALETTE[12],
        NamedColor::BrightMagenta => NAMED_COLOR_PALETTE[13],
        NamedColor::BrightCyan => NAMED_COLOR_PALETTE[14],
        NamedColor::BrightWhite => NAMED_COLOR_PALETTE[15],
        NamedColor::Foreground | NamedColor::BrightForeground => DEFAULT_FOREGROUND,
        NamedColor::Background => DEFAULT_BACKGROUND,
        NamedColor::Cursor => DEFAULT_FOREGROUND,
        NamedColor::DimForeground => dim(DEFAULT_FOREGROUND),
        NamedColor::DimBlack => dim(NAMED_COLOR_PALETTE[0]),
        NamedColor::DimRed => dim(NAMED_COLOR_PALETTE[1]),
        NamedColor::DimGreen => dim(NAMED_COLOR_PALETTE[2]),
        NamedColor::DimYellow => dim(NAMED_COLOR_PALETTE[3]),
        NamedColor::DimBlue => dim(NAMED_COLOR_PALETTE[4]),
        NamedColor::DimMagenta => dim(NAMED_COLOR_PALETTE[5]),
        NamedColor::DimCyan => dim(NAMED_COLOR_PALETTE[6]),
        NamedColor::DimWhite => dim(NAMED_COLOR_PALETTE[7]),
    }
}

/// The standard xterm 256-color cube (indices `16..=231`) and grayscale ramp (`232..=255`)
/// formulas - matches `vendor/zed/crates/terminal/src/terminal.rs`'s `get_color_at_index`/
/// `rgb_for_index` (a public xterm convention, cited from the same source there).
fn indexed_color_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => NAMED_COLOR_PALETTE[index as usize],
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

fn resolve_color(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Named(name) => named_color_rgb(name),
        Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => indexed_color_rgb(index),
    }
}

fn grid_cell_from_alacritty(cell: &AlacCell, is_cursor: bool) -> GridCell {
    let mut fg = resolve_color(cell.fg);
    let mut bg = resolve_color(cell.bg);
    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if is_cursor {
        std::mem::swap(&mut fg, &mut bg);
    }
    GridCell {
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
    /// fg/bg swapped, so the renderer doesn't need to separately overlay a cursor glyph.
    pub fn visible_rows(&self) -> Vec<Vec<GridCell>> {
        let content = self.term.renderable_content();
        let cursor_point =
            (content.cursor.shape != CursorShape::Hidden).then_some(content.cursor.point);

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
            row.push(grid_cell_from_alacritty(indexed.cell, is_cursor));
        }

        rows
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
        let rows = grid.visible_rows();
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
        let rows = grid.visible_rows();
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
        let rows = grid.visible_rows();
        assert!(row_text(&rows[0]).starts_with("SECOND"));
    }

    #[test]
    fn sgr_red_foreground_resolves_to_the_named_palette_color() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes(b"\x1b[31mR\x1b[0m");
        let rows = grid.visible_rows();
        assert_eq!(rows[0][0].c, 'R');
        assert_eq!(rows[0][0].fg, NAMED_COLOR_PALETTE[1]);
    }

    #[test]
    fn sgr_bold_sets_the_bold_flag() {
        let mut grid = TerminalGrid::new(2, 10);
        grid.append_bytes(b"\x1b[1mB");
        let rows = grid.visible_rows();
        assert!(rows[0][0].bold);
    }

    #[test]
    fn resize_changes_the_visible_row_and_column_count() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.resize(10, 40);
        let rows = grid.visible_rows();
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0].len(), 40);
    }

    #[test]
    fn clear_screen_erases_previously_written_text() {
        let mut grid = TerminalGrid::new(5, 20);
        grid.append_bytes(b"hello");
        grid.append_bytes(b"\x1b[2J\x1b[1;1H");
        let rows = grid.visible_rows();
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
        assert_eq!(row_text(&grid.visible_rows()[2]).trim(), "hello");

        grid.clear();
        for row in grid.visible_rows() {
            assert_eq!(
                row_text(&row).trim(),
                "",
                "clear must erase every visible row"
            );
        }

        // The cursor is homed to (1,1) - writing right after `clear()` lands at the top-left,
        // not wherever the cursor happened to be before.
        grid.append_bytes(b"X");
        assert_eq!(grid.visible_rows()[0][0].c, 'X');
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

        let rows = grid.visible_rows();
        assert_eq!(
            rows[1][2].c, 'O',
            "expected 'O' at row 2 col 3, got rows: {rows:?}"
        );
        assert_eq!(rows[1][3].c, 'K');
    }
}
