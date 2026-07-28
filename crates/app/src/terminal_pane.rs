//! A single terminal-backed pane: a real process (a plain shell, or - since step 4 - an
//! agent CLI like `claude`) spawned via `pty-core`, streamed into a real
//! `alacritty_terminal`-backed [`crate::terminal_grid::TerminalGrid`] and rendered as a
//! genuine cursor-addressed terminal grid (not a scrolling plain-text log - see
//! `crate::terminal_grid`'s module docs for why that distinction matters for agent CLIs).
//! What exactly gets spawned is described by [`TerminalSpec`] - `TerminalPane` itself has
//! no notion of "shell" vs. "agent CLI"; it just spawns whatever program/args/cwd it's
//! given. The session/tab bookkeeping that decides *which* `TerminalSpec` to use for a "New
//! Shell" vs. "New Claude Session" button, and that owns more than one `TerminalPane` at
//! once as tabs, lives in `crate::sessions`/`crate::root`, one layer up.
//!
//! ## Offloading blocking work off the GPUI foreground thread
//!
//! `pty_core::spawn` performs blocking I/O (opening a pty, spawning a child process), so it
//! is run via `cx.background_executor().spawn(..)` (GPUI's background thread pool - verified
//! at `vendor/zed/crates/gpui/src/executor.rs:89`, inside `impl BackgroundExecutor`; its
//! `timer(..)` sibling used below is at `:162`, same `impl` block), not called directly on
//! the foreground/UI thread. Once spawned, `PtySession::output()` is a bounded
//! `std::sync::mpsc::Receiver<Vec<u8>>` (see `pty-core`'s docs); rather than spawning a
//! second dedicated OS thread to bridge it, this drains it with the non-blocking
//! `try_recv()` from inside a GPUI foreground async task that wakes up every
//! [`POLL_INTERVAL`] via `cx.background_executor().timer(..)` - the same
//! "batch-drain-then-notify" shape Zed's own `terminal` crate uses for its PTY event loop
//! (`vendor/zed/crates/terminal/src/terminal.rs`, `cx.spawn` + `cx.background_executor().timer`).
//! `try_recv()` never blocks, so this is safe to call directly from the task driving
//! re-renders - but see [`MAX_CHUNKS_PER_TICK`] for why the drain loop itself is still
//! bounded, since "never blocks" isn't the same as "bounded work per call".
//!
//! ## Input
//!
//! Typed keys are forwarded to the real child process: [`TerminalPane`] is focusable
//! (`cx.focus_handle()` + `.track_focus(..)`, the same pattern
//! `vendor/zed/crates/terminal_view/src/terminal_view.rs` uses), registers
//! `.on_key_down(cx.listener(Self::handle_key_down))`, and [`keystroke_to_bytes`] turns a
//! `gpui::Keystroke` into the bytes a real terminal would send (printable characters,
//! Enter/Backspace/Tab/Escape/arrows, and `Ctrl`+letter control codes), written via
//! [`pty_core::PtySession::write_input`]. This is a deliberately small subset of
//! `vendor/zed/crates/terminal/src/mappings/keys.rs`'s `to_esc_str` (which itself needs
//! more `alacritty_terminal` terminal-mode state than this pane threads through for input
//! encoding - e.g. application cursor-key mode) - enough to type commands, use arrow keys,
//! and send `Ctrl-C`, not a full VT100 keymap.

use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use gpui::{
    canvas, div, font, prelude::*, rgb, Bounds, ClickEvent, Context, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, Keystroke, Pixels, Size, Task, Window,
};
use pty_core::{PtyError, PtySession, SpawnOptions};

use crate::terminal_grid::{GridCell, TerminalGrid, DEFAULT_BACKGROUND, DEFAULT_FOREGROUND};

/// How often the foreground poll task wakes up to drain any pty output that has arrived
/// and, if there was any, re-render. 33ms is close to a 30fps redraw rate: fast enough that
/// streaming shell output feels live, without re-rendering every single byte.
const POLL_INTERVAL: Duration = Duration::from_millis(33);

/// Defensive cap on how many output chunks a single poll tick will drain and decode
/// (fed into the real VT100 parser, see `crate::terminal_grid`) on the GPUI foreground
/// thread. Without this, a firehose child (e.g. `yes`, or a very chatty build tool) could
/// hand the poll loop the full contents of `pty-core`'s bounded output channel - up to
/// `OUTPUT_CHANNEL_CAPACITY * READ_BUF_SIZE` (~1MB; see `pty-core`'s docs) - to decode in a
/// single tick, on the same thread responsible for input handling and re-rendering. Capping
/// chunks-per-tick spreads that cost across multiple ticks instead: whatever isn't drained
/// this tick is still sitting in the channel (pty-core's reader thread just backpressures,
/// per its own docs) and gets picked up automatically on the next tick.
const MAX_CHUNKS_PER_TICK: usize = 64;

/// Initial pty size used for the spawned shell, before the first real resize (see
/// `maybe_resize_pty`) has a chance to run during the first render.
const TERMINAL_ROWS: u16 = 48;
const TERMINAL_COLS: u16 = 160;

/// Approximate monospace cell metrics, in pixels, for the `text_xs()` font size this pane
/// renders with. Used only to turn a pixel viewport into an approximate row/column count
/// for `PtySession::resize` and `TerminalGrid::resize` - not measured from the actual
/// font/renderer (no verified GPUI API for querying a rendered glyph's real advance width
/// was used for this step), so treat this as a reasonable estimate, not a pixel-accurate
/// fit.
const APPROX_CELL_WIDTH_PX: f32 = 7.0;
const APPROX_CELL_HEIGHT_PX: f32 = 16.0;

/// What a [`TerminalPane`] should spawn: a program, its arguments, and the working
/// directory to spawn it in. Generalizes step 3's "always spawn `$SHELL`" so the same pane
/// implementation can host a plain shell *or* an agent CLI (`claude`, `codex`, ...) - see
/// the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl TerminalSpec {
    /// The user's shell (`$SHELL`, falling back to `/bin/bash` if unset), no extra
    /// arguments - this is exactly step 3's original default-shell behavior, just factored
    /// out so it's one `TerminalSpec` variant among several instead of the only option.
    pub fn shell(cwd: PathBuf) -> Self {
        let program = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/bash"));
        Self {
            program,
            args: Vec::new(),
            cwd,
        }
    }

    /// An arbitrary command. `program` may be a bare name (e.g. `"claude"`, with no path
    /// separator): `pty-core`'s `spawn` resolves it via `PATH` through
    /// `portable_pty::CommandBuilder` the same way a shell would (verified against
    /// `portable-pty-0.9.0`'s `cmdbuilder.rs`, and already exercised by `pty-core`'s own
    /// `spawns_and_reads_short_process_output` test, which spawns bare `"echo"`), so this
    /// doesn't need the caller to resolve an absolute path itself.
    pub fn command(program: impl Into<PathBuf>, args: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            program: program.into(),
            args,
            cwd,
        }
    }
}

pub struct TerminalPane {
    spec: TerminalSpec,
    grid: TerminalGrid,
    session: Option<PtySession>,
    spawn_error: Option<String>,
    focus_handle: FocusHandle,
    /// This pane's own real, rendered content-area bounds - captured every frame via a
    /// measuring `canvas()` child in `render` (see that method's docs for why this exists
    /// instead of `window.viewport_size()`). `None` only before the very first paint.
    content_bounds: Option<Bounds<Pixels>>,
    /// Tracks which `(rows, cols)` the grid and the real child pty are each actually in
    /// sync with - see [`ResizeLatch`]'s docs for the real bug this decomposition exists
    /// to prevent.
    resize_latch: ResizeLatch,
    /// Owns the in-flight "spawn the process, then poll its output" task. Dropping/replacing
    /// this cancels whatever the previous task was doing, which is what stops an old
    /// session's poll loop from racing a new one over the same struct fields.
    _task: Option<Task<()>>,
}

impl TerminalPane {
    pub fn new(spec: TerminalSpec, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            spec,
            grid: TerminalGrid::new(TERMINAL_ROWS, TERMINAL_COLS),
            session: None,
            spawn_error: None,
            focus_handle: cx.focus_handle(),
            content_bounds: None,
            resize_latch: ResizeLatch::default(),
            _task: None,
        };
        this.spawn_process(cx);
        this
    }

    /// Deterministically tears down the current child process, if any, via the real
    /// `PtySession::shutdown` (blocks until the process tree is confirmed dead and reaped;
    /// see `pty-core`'s "Kill: `Drop` vs. `shutdown()`" docs) - run on the background
    /// executor so this doesn't block the GPUI foreground thread. Intended for closing a
    /// tab: called before the owning `Entity<TerminalPane>` is dropped, so process teardown
    /// is a completed, verified fact rather than left to `Drop`'s fire-and-forget
    /// signal-then-detach behavior (which is still correct and non-leaking on its own, per
    /// `pty-core`'s docs, just not deterministic about *when* the process is fully reaped).
    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        self._task = None;
        if let Some(mut session) = self.session.take() {
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = session.shutdown() {
                        log::warn!("failed to shut down terminal session: {err}");
                    }
                })
                .detach();
        }
    }

    fn spawn_process(&mut self, cx: &mut Context<Self>) {
        let spec = self.spec.clone();
        let program_for_error = spec.program.clone();
        let task = cx.spawn(async move |this, cx| {
            let spawn_result: Result<PtySession, PtyError> = cx
                .background_executor()
                .spawn(async move {
                    pty_core::spawn(
                        SpawnOptions::new(spec.program)
                            .args(spec.args)
                            .cwd(spec.cwd)
                            .size(TERMINAL_ROWS, TERMINAL_COLS),
                    )
                })
                .await;

            let session = match spawn_result {
                Ok(session) => session,
                Err(err) => {
                    let message = format!("failed to start {}: {err}", program_for_error.display());
                    let _ = this.update(cx, |this, cx| {
                        this.spawn_error = Some(message);
                        cx.notify();
                    });
                    return;
                }
            };

            if this
                .update(cx, |this, cx| {
                    this.session = Some(session);
                    // The pane may already have rendered (and computed a target grid size)
                    // before this task's background spawn finished - see `ResizeLatch`'s
                    // docs for why that earlier call could not have reached the pty (there
                    // was no live session yet) and why it's retried here now that one
                    // exists, rather than waiting for the next window/pane resize to ever
                    // reach the real child pty.
                    if let Some(target) = this.resize_latch.grid {
                        this.resize_to(target.0, target.1);
                    }
                    cx.notify();
                })
                .is_err()
            {
                return; // the pane was dropped before the process finished starting
            }

            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;

                let poll_result = this.update(cx, |this, cx| {
                    let mut appended = false;
                    let mut process_ended = false;

                    if let Some(session) = &this.session {
                        // Capped at `MAX_CHUNKS_PER_TICK`, not drained to empty: see that
                        // constant's docs for why an unbounded drain here is a real
                        // foreground-thread-starvation risk against a firehose child.
                        // Anything left in the channel just gets picked up next tick.
                        for _ in 0..MAX_CHUNKS_PER_TICK {
                            match session.output().try_recv() {
                                Ok(chunk) => {
                                    this.grid.append_bytes(&chunk);
                                    appended = true;
                                }
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Disconnected) => {
                                    process_ended = true;
                                    break;
                                }
                            }
                        }
                    }

                    if process_ended {
                        this.session = None;
                        this.grid.mark_ended();
                        appended = true;
                    }

                    if appended {
                        cx.notify();
                    }

                    !process_ended
                });

                match poll_result {
                    Ok(true) => continue,
                    Ok(false) => break, // the child process exited; nothing left to poll
                    Err(_) => break,    // the pane entity was dropped
                }
            }
        });

        self._task = Some(task);
    }

    /// Forwards a typed key to the real child process via `PtySession::write_input`. See
    /// the module docs' "Input" section for the (deliberately small) subset of keys
    /// handled.
    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        let Some(bytes) = keystroke_to_bytes(&event.keystroke) else {
            return;
        };
        if let Err(err) = session.write_input(&bytes) {
            self.spawn_error = Some(format!("failed to write input: {err}"));
            cx.notify();
        }
        // Consumed as terminal input; don't let it also be interpreted as an app-level
        // keybinding (matches `vendor/zed/crates/terminal_view/src/terminal_view.rs`'s
        // `key_down` handler, which does the same after a successful `try_keystroke`).
        cx.stop_propagation();
    }

    /// Recomputes an approximate `(rows, cols)` from this pane's own real content-area
    /// bounds (see [`Self::content_bounds`]'s docs) and applies it via [`Self::resize_to`].
    /// Called from `render` so it naturally re-runs whenever the pane's own size changes -
    /// not just whenever the *window's* size changes, since Phase A's three-zone shell
    /// means those are no longer the same thing (see [`size_to_grid`]'s docs for the real
    /// bug this distinction fixes).
    ///
    /// [`Self::content_bounds`] reflects the *previous* frame's measured bounds (the
    /// measuring `canvas()` in `render` only fires during paint, which happens after
    /// `render` itself returns) - one frame stale on the very first resize, which
    /// self-corrects on the next render and is a normal, accepted trade-off for GPUI's
    /// real bounds-measurement idiom (verified against `vendor/zed/crates/workspace/
    /// src/workspace.rs`'s own `this.bounds = bounds` canvas pattern, which has the same
    /// one-frame lag). Before any paint has happened at all, falls back to
    /// `window.viewport_size()` - a real, if too-wide, guess that's still strictly better
    /// than not resizing at all, and gets corrected by the first real measurement.
    fn maybe_resize_pty(&mut self, window: &Window) {
        let size = self
            .content_bounds
            .map(|bounds| bounds.size)
            .unwrap_or_else(|| window.viewport_size());
        let (rows, cols) = size_to_grid(size);
        self.resize_to(rows, cols);
    }

    /// Applies a target `(rows, cols)` to the grid and, if a live session exists, the real
    /// child pty - delegating the "what actually needs to happen" decision to
    /// [`ResizeLatch::apply`] (see its docs for the bug this split exists to prevent), and
    /// only calling [`ResizeLatch::session_resize_succeeded`] once `PtySession::resize` has
    /// actually returned `Ok`, so a failed resize is retried next time instead of being
    /// permanently (and incorrectly) treated as done.
    fn resize_to(&mut self, rows: u16, cols: u16) {
        let actions = self
            .resize_latch
            .apply((rows, cols), self.session.is_some());

        if actions.resize_grid {
            self.grid.resize(rows, cols);
        }

        if actions.resize_session {
            let Some(session) = &self.session else {
                return;
            };
            match session.resize(rows, cols) {
                Ok(()) => self.resize_latch.session_resize_succeeded((rows, cols)),
                Err(err) => log::warn!("failed to resize pty session: {err}"),
            }
        }
    }
}

/// Converts a pixel-space size into an approximate `(rows, cols)` terminal grid size using
/// [`APPROX_CELL_WIDTH_PX`]/[`APPROX_CELL_HEIGHT_PX`] - not measured from the actual
/// font/renderer (no verified GPUI API for querying a rendered glyph's real advance width
/// was used for this step), so treat this as a reasonable estimate, not a pixel-accurate
/// fit. Deliberately a pure function of a [`Size<Pixels>`], independent of `Window` or this
/// pane's own state, so it's directly unit-testable (see the tests below) rather than only
/// exercisable through a real GPUI window.
///
/// This function itself isn't what the real bug was - it faithfully turns whatever size
/// it's given into a column/row count. The bug (see `TerminalPane::maybe_resize_pty`'s
/// history) was in *what size* it used to be called with: the *whole window's*
/// `viewport_size()`, unconditionally. Phase A's three-zone shell added a 276px rail, a
/// 320px panel, and ~64px of title/status-bar chrome around the centre pane, none of which
/// this pane's own content occupies - at a 1440px-wide window that computed roughly 205
/// columns even though the real visible terminal pane is only around 820-840px wide
/// (roughly 118-120 columns at [`APPROX_CELL_WIDTH_PX`]). The practical effect: every
/// terminal row rendered assuming ~205 columns were visible when only ~118 actually were,
/// so any full-width line silently rendered ~42% off the right edge of the pane - not
/// clipped-and-correct, just invisible. Fixed by calling this with this pane's own real,
/// measured content-area size (`TerminalPane::content_bounds`) instead of the window's.
fn size_to_grid(size: Size<Pixels>) -> (u16, u16) {
    let cols = ((size.width.as_f32() / APPROX_CELL_WIDTH_PX) as u16).max(20);
    let rows = ((size.height.as_f32() / APPROX_CELL_HEIGHT_PX) as u16).max(10);
    (rows, cols)
}

/// What [`ResizeLatch::apply`] says the caller should actually do for a target size.
#[derive(Debug, PartialEq, Eq)]
struct ResizeActions {
    resize_grid: bool,
    resize_session: bool,
}

/// Tracks, separately, which `(rows, cols)` [`TerminalGrid`] currently reflects and which
/// `(rows, cols)` was last successfully sent to a *live* `PtySession::resize`.
///
/// This split exists because of a real, empirically-confirmed bug: a single latched "last
/// size" field, set unconditionally on every call (including before any `PtySession`
/// existed - `TerminalPane::new` returns with `session: None`, since spawning happens
/// asynchronously on the background executor), meant the very first render latched a
/// target size while there was no session to resize yet, and every later call with that
/// same computed size then short-circuited on "already up to date" - permanently, even
/// once a real session appeared. The real child pty stayed stuck at
/// `TERMINAL_ROWS`/`TERMINAL_COLS` (its spawn-time size) forever, silently diverging from
/// what `TerminalGrid` (and the rendered UI) believed the size was - confirmed empirically
/// via `stty -F <pty> size` against a live session's actual pty. Any full-screen,
/// cursor-addressed program (`vim`, `less`, an agent CLI's own interactive UI) would then
/// paint for the wrong dimensions.
///
/// [`Self::grid`] is latched unconditionally (resizing `TerminalGrid` has no failure mode
/// and no live-session precondition), but [`Self::session`] is latched *only* by
/// [`Self::session_resize_succeeded`] - called by `TerminalPane::resize_to` only after a
/// real `PtySession::resize` call has actually returned `Ok`. A target size computed
/// before a session exists therefore never gets latched as "session in sync", so
/// `TerminalPane::spawn_process`'s success callback re-running `resize_to` at that same
/// cached target size (see its own doc comment) genuinely reaches the pty instead of being
/// skipped.
#[derive(Debug, Default)]
struct ResizeLatch {
    /// The `(rows, cols)` `TerminalGrid` currently reflects.
    grid: Option<(u16, u16)>,
    /// The `(rows, cols)` last successfully sent to a *live* session's real pty resize -
    /// `None` until a session exists and a resize has actually reached it.
    session: Option<(u16, u16)>,
}

impl ResizeLatch {
    /// Decides what `(rows, cols)` needs applying to the grid and/or a live session's real
    /// pty, and latches [`Self::grid`] immediately (grid resizes can't fail). Does *not*
    /// latch [`Self::session`] - only [`Self::session_resize_succeeded`] does that, once
    /// the caller has confirmed a real `PtySession::resize` call actually succeeded.
    fn apply(&mut self, target: (u16, u16), has_session: bool) -> ResizeActions {
        let resize_grid = self.grid != Some(target);
        if resize_grid {
            self.grid = Some(target);
        }

        let resize_session = has_session && self.session != Some(target);

        ResizeActions {
            resize_grid,
            resize_session,
        }
    }

    /// Records that `target` has actually reached a live session's real pty via a
    /// successful `PtySession::resize` call - only after this has it been called does
    /// [`Self::apply`] treat `target` as already in sync for the session side.
    fn session_resize_succeeded(&mut self, target: (u16, u16)) {
        self.session = Some(target);
    }
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Converts a typed key into the bytes a real terminal would send for it. A deliberately
/// small subset of `vendor/zed/crates/terminal/src/mappings/keys.rs`'s `to_esc_str` - see
/// the module docs' "Input" section for why the full mapping isn't replicated here.
/// Returns `None` for keys with no reasonable terminal-input meaning (e.g. a bare modifier
/// key, or a function key this subset doesn't handle), in which case nothing is sent.
fn keystroke_to_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    // Ctrl+<letter> control codes (Ctrl-A through Ctrl-Z), e.g. Ctrl-C -> 0x03 (SIGINT at
    // the line discipline), Ctrl-D -> 0x04 (EOF). Computed rather than hardcoded per-key:
    // this is the standard terminal mapping (`letter.to_ascii_uppercase() as u8 & 0x1f`).
    if keystroke.modifiers.control && !keystroke.modifiers.alt && !keystroke.modifiers.platform {
        if let Some(ch) = keystroke.key.chars().next() {
            if keystroke.key.chars().count() == 1 && ch.is_ascii_alphabetic() {
                let code = (ch.to_ascii_uppercase() as u8) & 0x1f;
                return Some(vec![code]);
            }
        }
    }

    match keystroke.key.as_str() {
        "enter" => Some(b"\r".to_vec()),
        "backspace" => Some(b"\x7f".to_vec()),
        "tab" => Some(b"\t".to_vec()),
        "escape" => Some(b"\x1b".to_vec()),
        "up" => Some(b"\x1b[A".to_vec()),
        "down" => Some(b"\x1b[B".to_vec()),
        "right" => Some(b"\x1b[C".to_vec()),
        "left" => Some(b"\x1b[D".to_vec()),
        "space" => Some(b" ".to_vec()),
        _ => keystroke
            .key_char
            .as_ref()
            .filter(|text| !text.is_empty())
            .map(|text| text.as_bytes().to_vec()),
    }
}

/// Packs an `(r, g, b)` triple into the `0xRRGGBB` form `gpui::rgb` expects.
fn pack_rgb((r, g, b): (u8, u8, u8)) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Whether two cells share every attribute a rendered run cares about (everything but the
/// character itself) - used to group a grid row into as few styled spans as possible.
fn same_run_style(a: &GridCell, b: &GridCell) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.strikethrough == b.strikethrough
}

/// Renders one grid row as a horizontal run of styled spans - grouping consecutive cells
/// that share the same style into a single span keeps the element count low (a typical row
/// is mostly-uniform default-styled text, so this is usually 1-3 spans, not one element per
/// character) even though the underlying grid can be up to `TERMINAL_ROWS` x
/// `TERMINAL_COLS` cells.
fn render_row(row: &[GridCell]) -> impl IntoElement {
    let mut line = div().flex().flex_row();

    let mut start = 0;
    while start < row.len() {
        let style = &row[start];
        let mut end = start + 1;
        while end < row.len() && same_run_style(&row[end], style) {
            end += 1;
        }

        let text: String = row[start..end].iter().map(|cell| cell.c).collect();
        let mut span = div().text_color(rgb(pack_rgb(style.fg)));
        if style.bg != DEFAULT_BACKGROUND {
            span = span.bg(rgb(pack_rgb(style.bg)));
        }
        if style.bold {
            span = span.font_weight(FontWeight::BOLD);
        }
        if style.italic {
            span = span.italic();
        }
        if style.underline {
            span = span.underline();
        }
        if style.strikethrough {
            span = span.line_through();
        }
        line = line.child(span.child(text));

        start = end;
    }

    line
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.maybe_resize_pty(window);

        // Measures this pane's own real, rendered content-area bounds every frame, so
        // `Self::maybe_resize_pty` can size the terminal grid from the pane's actual
        // width/height instead of the whole window's - see that method's docs and
        // `size_to_grid`'s docs for the real bug this fixes. `.absolute().size_full()`
        // inside a `.relative()` parent, updating `self.content_bounds` from the real
        // `canvas()` prepaint callback via a strong `Entity<Self>` handle, is the same real
        // idiom `vendor/zed/crates/workspace/src/workspace.rs` uses for its own dock-sizing
        // bounds (`let this = cx.entity(); canvas(move |bounds, _, cx| { this.update(cx, ..)
        // }, ..).absolute().size_full()`), not an invented pattern.
        let measure_bounds = {
            let this = cx.entity();
            canvas(
                move |bounds, _window, cx| {
                    this.update(cx, |this, _cx| {
                        this.content_bounds = Some(bounds);
                    });
                },
                |_bounds, _prepaint, _window, _cx| {},
            )
            .absolute()
            .size_full()
        };

        let mut pane = div()
            .id("terminal-pane")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
            }))
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .bg(rgb(pack_rgb(DEFAULT_BACKGROUND)))
            .p_2()
            // The real bundled IBM Plex Mono (`crate::fonts`), not a generic "monospace"
            // alias - the terminal is this app's most prominent monospace surface, so it's
            // the clearest place to prove the bundled font is actually rendering.
            .font(font(crate::theme::font::MONO))
            .text_xs()
            .text_color(rgb(pack_rgb(DEFAULT_FOREGROUND)))
            .child(measure_bounds);

        if let Some(error) = &self.spawn_error {
            pane = pane.child(
                div()
                    .text_color(rgb(0xff6b6b))
                    .child(format!("failed to start process: {error}")),
            );
        }

        for row in self.grid.visible_rows() {
            pane = pane.child(render_row(&row));
        }

        if self.grid.ended {
            pane = pane.child(div().text_color(rgb(0xffcc66)).child("[process exited]"));
        }

        pane
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;
    use gpui::{px, size};

    #[test]
    fn size_to_grid_derives_columns_and_rows_from_the_given_size_not_a_fixed_constant() {
        // A plausible centre-pane content width once Phase A's shell chrome (a 276px rail
        // plus a 320px panel plus borders) is subtracted from a 1440px window - roughly
        // 820px, not the full 1440.
        let (rows, cols) = size_to_grid(size(px(820.0), px(800.0)));
        assert_eq!(cols, (820.0 / APPROX_CELL_WIDTH_PX) as u16);
        assert_eq!(rows, (800.0 / APPROX_CELL_HEIGHT_PX) as u16);
    }

    #[test]
    fn size_to_grid_enforces_a_minimum_row_and_column_count() {
        let (rows, cols) = size_to_grid(size(px(10.0), px(10.0)));
        assert_eq!(cols, 20);
        assert_eq!(rows, 10);
    }

    #[test]
    fn size_to_grid_from_a_real_pane_width_is_plausible_not_the_full_window_derived_count() {
        // Regression guard documenting the actual magnitude of the original bug: deriving
        // columns from the *whole* 1440px window (ignoring the 276+320px of shell chrome
        // either side) computed roughly 205 columns; the real, visible centre-pane width is
        // roughly 820-840px, around 118-120 columns. Both numbers are "correct" outputs of
        // `size_to_grid` for their respective inputs - the bug was `maybe_resize_pty`
        // feeding it the wrong one. This test pins the two magnitudes apart so a future
        // regression back to `window.viewport_size()` would be caught by inspection here.
        let (_rows, whole_window_cols) = size_to_grid(size(px(1440.0), px(928.0)));
        let (_rows, real_pane_cols) = size_to_grid(size(px(828.0), px(800.0)));
        assert!(
            whole_window_cols > real_pane_cols * 3 / 2,
            "expected the whole-window column count ({whole_window_cols}) to be \
             substantially larger than the real-pane-width column count ({real_pane_cols}) \
             - if they're close, something about the approximation changed"
        );
        assert!(real_pane_cols < 130, "got {real_pane_cols}");
    }

    #[test]
    fn first_apply_with_no_session_resizes_the_grid_but_not_a_session() {
        let mut latch = ResizeLatch::default();
        let actions = latch.apply((48, 160), false);
        assert!(actions.resize_grid);
        assert!(!actions.resize_session);
    }

    /// The exact regression this whole module exists to prevent: a target size computed
    /// before any session exists must still trigger a real session resize once one
    /// appears, not be silently treated as already in sync.
    #[test]
    fn a_size_computed_before_any_session_exists_is_retried_once_one_appears() {
        let mut latch = ResizeLatch::default();
        let target = (48, 160);

        let first = latch.apply(target, false);
        assert!(first.resize_grid);
        assert!(!first.resize_session);

        let second = latch.apply(target, true);
        assert!(
            second.resize_session,
            "a session appearing at an already-computed size must still trigger a real \
             session resize, not be treated as already in sync"
        );
    }

    #[test]
    fn does_not_repeat_a_successful_session_resize_for_the_same_target() {
        let mut latch = ResizeLatch::default();
        let target = (48, 160);
        latch.apply(target, true);
        latch.session_resize_succeeded(target);

        let repeat = latch.apply(target, true);
        assert!(
            !repeat.resize_session,
            "already in sync at this size; resizing again would be redundant"
        );
    }

    #[test]
    fn a_failed_session_resize_is_retried_since_it_was_never_latched_as_succeeded() {
        let mut latch = ResizeLatch::default();
        let target = (48, 160);
        latch.apply(target, true);
        // Simulate a failed `PtySession::resize` call: the caller deliberately does NOT
        // call `session_resize_succeeded` in this path.

        let retry = latch.apply(target, true);
        assert!(
            retry.resize_session,
            "a failed resize must not be latched as done, or it would never be retried"
        );
    }

    #[test]
    fn a_new_target_size_resizes_both_grid_and_session_again() {
        let mut latch = ResizeLatch::default();
        latch.apply((48, 160), true);
        latch.session_resize_succeeded((48, 160));

        let actions = latch.apply((50, 170), true);
        assert!(actions.resize_grid);
        assert!(actions.resize_session);
    }
}
