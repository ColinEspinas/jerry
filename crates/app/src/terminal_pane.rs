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
    div, font, prelude::*, rgb, ClickEvent, Context, FocusHandle, Focusable, FontWeight,
    KeyDownEvent, Keystroke, Task, Window,
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
    /// The `(rows, cols)` last sent to `PtySession::resize`/`TerminalGrid::resize`, so
    /// `maybe_resize_pty` only issues a real resize call when the computed size actually
    /// changes instead of on every render.
    last_size: Option<(u16, u16)>,
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
            last_size: None,
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

    /// Recomputes an approximate `(rows, cols)` from the window's viewport (see
    /// `APPROX_CELL_WIDTH_PX`/`APPROX_CELL_HEIGHT_PX`'s docs for the caveat that this is
    /// the whole window, not this pane's own element bounds) and issues real
    /// `PtySession::resize`/`TerminalGrid::resize` calls when it changed - both must move
    /// together (see `TerminalGrid::resize`'s docs) or the rendered grid's geometry would
    /// silently diverge from what the child process believes its terminal size is. Called
    /// from `render` so it naturally re-runs whenever the window is resized.
    fn maybe_resize_pty(&mut self, window: &Window) {
        let viewport = window.viewport_size();
        let cols = ((viewport.width.as_f32() / APPROX_CELL_WIDTH_PX) as u16).max(20);
        let rows = ((viewport.height.as_f32() / APPROX_CELL_HEIGHT_PX) as u16).max(10);

        if self.last_size == Some((rows, cols)) {
            return;
        }
        self.last_size = Some((rows, cols));

        self.grid.resize(rows, cols);
        if let Some(session) = &self.session {
            if let Err(err) = session.resize(rows, cols) {
                log::warn!("failed to resize pty session: {err}");
            }
        }
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

        let mut pane = div()
            .id("terminal-pane")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(rgb(pack_rgb(DEFAULT_BACKGROUND)))
            .p_2()
            .font(font("monospace"))
            .text_xs()
            .text_color(rgb(pack_rgb(DEFAULT_FOREGROUND)));

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
