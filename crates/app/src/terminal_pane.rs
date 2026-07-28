//! The center pane: a real shell spawned via `pty-core`, streamed into a
//! [`crate::ansi::TerminalBuffer`] and rendered as scrolling monospace text.
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
//! `alacritty_terminal`'s terminal-mode state this crate doesn't have - see `crate::ansi`'s
//! scope decision) - enough to type commands and send `Ctrl-C`, not a full VT100 keymap.

use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use gpui::{
    div, font, prelude::*, rgb, ClickEvent, Context, FocusHandle, Focusable, KeyDownEvent,
    Keystroke, ScrollHandle, Task, Window,
};
use pty_core::{PtyError, PtySession, SpawnOptions};

use crate::ansi::TerminalBuffer;

/// How often the foreground poll task wakes up to drain any pty output that has arrived
/// and, if there was any, re-render. 33ms is close to a 30fps redraw rate: fast enough that
/// streaming shell output feels live, without re-rendering every single byte.
const POLL_INTERVAL: Duration = Duration::from_millis(33);

/// Defensive cap on how many output chunks a single poll tick will drain and decode
/// (ANSI-strip + append into `TerminalBuffer`) on the GPUI foreground thread. Without this,
/// a firehose child (e.g. `yes`, or a very chatty build tool) could hand the poll loop the
/// full contents of `pty-core`'s bounded output channel - up to `OUTPUT_CHANNEL_CAPACITY *
/// READ_BUF_SIZE` (~1MB; see `pty-core`'s docs) - to decode in a single tick, on the same
/// thread responsible for input handling and re-rendering. Capping chunks-per-tick spreads
/// that cost across multiple ticks instead: whatever isn't drained this tick is still
/// sitting in the channel (pty-core's reader thread just backpressures, per its own docs)
/// and gets picked up automatically on the next tick.
const MAX_CHUNKS_PER_TICK: usize = 64;

/// Initial pty size used for the spawned shell, before the first real resize (see
/// `maybe_resize_pty`) has a chance to run during the first render.
const TERMINAL_ROWS: u16 = 48;
const TERMINAL_COLS: u16 = 160;

/// Approximate monospace cell metrics, in pixels, for the `text_xs()` font size this pane
/// renders with. Used only to turn a pixel viewport into an approximate row/column count
/// for `PtySession::resize` - not measured from the actual font/renderer (no verified GPUI
/// API for querying a rendered glyph's real advance width was used for this step), so
/// treat this as a reasonable estimate, not a pixel-accurate fit.
const APPROX_CELL_WIDTH_PX: f32 = 7.0;
const APPROX_CELL_HEIGHT_PX: f32 = 16.0;

/// Only the most recent lines are rendered as elements each frame; the buffer itself can
/// hold far more (see `crate::ansi::MAX_LINES`), but rendering thousands of `div`s per
/// frame for scrollback that's off-screen anyway isn't worth doing in this step.
const MAX_RENDERED_LINES: usize = 1000;

pub struct TerminalPane {
    cwd: PathBuf,
    buffer: TerminalBuffer,
    session: Option<PtySession>,
    spawn_error: Option<String>,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
    /// The `(rows, cols)` last sent to `PtySession::resize`, so `maybe_resize_pty` only
    /// issues a real resize call when the computed size actually changes instead of on
    /// every render.
    last_size: Option<(u16, u16)>,
    /// Owns the in-flight "spawn the shell, then poll its output" task. Replacing this
    /// (see `respawn`) drops/cancels whatever the previous task was doing, which is what
    /// stops an old worktree's poll loop from racing the new one over the same struct
    /// fields after a worktree switch.
    _task: Option<Task<()>>,
}

impl TerminalPane {
    pub fn new(cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            cwd,
            buffer: TerminalBuffer::new(),
            session: None,
            spawn_error: None,
            scroll_handle: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            last_size: None,
            _task: None,
        };
        this.spawn_shell(cx);
        this
    }

    /// Tears down the current shell (a fast, non-blocking `Drop` per `pty-core`'s docs -
    /// safe to do directly on the foreground thread) and starts a new one in `cwd`.
    pub fn respawn(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        self.cwd = cwd;
        self._task = None;
        self.session = None;
        self.buffer = TerminalBuffer::new();
        self.spawn_error = None;
        // Force `maybe_resize_pty` to issue a real resize against the *new* session on the
        // next render, even if the computed size happens to match the old session's last
        // known size - the new `PtySession` starts at `TERMINAL_ROWS`/`TERMINAL_COLS`
        // regardless of what the old one had been resized to.
        self.last_size = None;
        self.spawn_shell(cx);
        cx.notify();
    }

    fn spawn_shell(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        let task = cx.spawn(async move |this, cx| {
            let shell = std::env::var_os("SHELL")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/bin/bash"));

            let spawn_result: Result<PtySession, PtyError> = cx
                .background_executor()
                .spawn(async move {
                    pty_core::spawn(
                        SpawnOptions::new(shell)
                            .cwd(cwd)
                            .size(TERMINAL_ROWS, TERMINAL_COLS),
                    )
                })
                .await;

            let session = match spawn_result {
                Ok(session) => session,
                Err(err) => {
                    let message = err.to_string();
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
                return; // the pane was dropped before the shell finished starting
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
                                    this.buffer.append_bytes(&chunk);
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
                        this.buffer.mark_ended();
                        appended = true;
                    }

                    if appended {
                        // Pin to the bottom exactly when new output actually arrived, not
                        // on every render pass (which would re-run this against whatever
                        // scroll position the user had just set by scrolling up to read
                        // back through history - see the `Render` impl below).
                        this.scroll_handle.scroll_to_bottom();
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
    /// the whole window, not this pane's own element bounds) and issues a real
    /// `PtySession::resize` when it changed. Called from `render` so it naturally re-runs
    /// whenever the window is resized.
    fn maybe_resize_pty(&mut self, window: &Window) {
        let viewport = window.viewport_size();
        let cols = ((viewport.width.as_f32() / APPROX_CELL_WIDTH_PX) as u16).max(20);
        let rows = ((viewport.height.as_f32() / APPROX_CELL_HEIGHT_PX) as u16).max(10);

        if self.last_size == Some((rows, cols)) {
            return;
        }
        self.last_size = Some((rows, cols));

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

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.maybe_resize_pty(window);

        let mut pane = div()
            .id("terminal-pane")
            .track_focus(&self.focus_handle)
            .track_scroll(&self.scroll_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(0x1e1e1e))
            .p_2()
            .font(font("monospace"))
            .text_xs()
            .text_color(rgb(0xd4d4d4));

        pane = pane.child(
            div()
                .text_xs()
                .text_color(rgb(0x7a7a7a))
                .child(format!("$ {}", self.cwd.display())),
        );

        if let Some(error) = &self.spawn_error {
            pane = pane.child(
                div()
                    .text_color(rgb(0xff6b6b))
                    .child(format!("failed to start shell: {error}")),
            );
        }

        let lines: Vec<&str> = self.buffer.lines().collect();
        let start = lines.len().saturating_sub(MAX_RENDERED_LINES);
        for line in &lines[start..] {
            pane = pane.child(div().child(line.to_string()));
        }

        if self.buffer.ended {
            pane = pane.child(div().text_color(rgb(0xffcc66)).child("[process exited]"));
        }

        // Note: pinning scroll to the bottom happens in the poll loop above, exactly when
        // new output actually arrived - not unconditionally here on every render (which
        // used to fight the user scrolling up to read back through history; see the poll
        // loop's comment).
        pane
    }
}
