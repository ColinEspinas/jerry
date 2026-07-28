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
use std::time::{Duration, Instant};

use gpui::{
    canvas, div, font, prelude::*, px, rgb, Bounds, ClickEvent, Context, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, Keystroke, Pixels, Size, Task, Window,
};
use pty_core::{ExitStatus, PtyError, PtySession, SpawnOptions};

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

/// How many [`POLL_INTERVAL`] ticks (~10s total) [`TerminalPane`]'s poll loop keeps retrying
/// `PtySession::try_wait` after observing pty EOF before giving up - see
/// [`eof_poll_decision`]'s docs for the real race this bounds, and why giving up is a
/// synthetic failure rather than a silent "no status".
const MAX_EOF_POLL_TICKS: u32 = 300;

/// Decides what a poll tick should do once pty EOF has been observed (the output channel's
/// `TryRecvError::Disconnected`) but the child's real exit status hasn't been confirmed yet,
/// given this tick's own non-blocking `PtySession::try_wait` result.
///
/// This is a pure decision function, factored out of `TerminalPane::spawn_process`'s poll
/// loop (mirroring [`ResizeLatch`]'s own split of "what changed" from "what to actually do
/// about it") specifically so the real, checker-reproduced bug this fixes is directly
/// unit-testable without a real GPUI window or timing a real poll loop against a wall clock:
/// the original code called `try_wait` exactly **once**, at the moment EOF was observed, and
/// if that single non-blocking check returned `Ok(None)` (the child hasn't been reaped
/// *yet*, but is still alive) it gave up immediately, dropped the `PtySession` (which, per
/// `pty-core`'s own `Drop` impl, *signals the still-live process* - so a legitimate,
/// still-running child got killed out from under itself), and left `exit_status` `None`
/// forever, which `crate::status::derive_status` then reads as [`crate::status::
/// ProcessSignal::NoProcess`] - i.e. `Status::Idle`, not `Status::Fail`. This is a real,
/// reproducible race, not theoretical: any child that closes its own pty-attached stdio
/// before actually exiting (`sh -c 'exec 0<&- 1>&- 2>&-; sleep 2; exit 7'` is a minimal
/// repro - see the real end-to-end test below) triggers EOF well before it's reapable.
///
/// Returns `None` when the caller should keep the session alive and retry next tick
/// (`try_wait` hasn't resolved yet, or errored transiently, and the tick budget isn't
/// exhausted) - critically, `None` here must *not* cause the caller to drop the
/// `PtySession`. Returns `Some(status)` once the caller should finalize: either the real,
/// confirmed [`ExitStatus`], or - once [`MAX_EOF_POLL_TICKS`] is exhausted without ever
/// confirming one - a synthetic failed status (`ExitStatus::with_signal`, a real, public
/// `portable_pty` constructor; its `success()` is always `false`), so a process whose exit
/// could never be confirmed is reported as [`crate::status::Status::Fail`] rather than
/// silently reverting to [`crate::status::Status::Idle`].
fn eof_poll_decision(
    try_wait_result: Result<Option<ExitStatus>, ()>,
    ticks_pending: u32,
) -> Option<ExitStatus> {
    match try_wait_result {
        Ok(Some(status)) => Some(status),
        Ok(None) | Err(()) => {
            if ticks_pending >= MAX_EOF_POLL_TICKS {
                Some(ExitStatus::with_signal(
                    "gave up waiting for exit status after EOF",
                ))
            } else {
                None
            }
        }
    }
}

/// The terminal body's real, explicit font size and line height -
/// `design_handoff_jerry_ade/README.md`'s Surface A/B body spec: "lines at 12px/19 mono".
/// Set explicitly (`.text_size()`/`.line_height()`) on the pane root by [`TerminalPane`]'s
/// own `Render` impl, not per-row by [`render_row`] - each row's children simply inherit both
/// from that root, the same real GPUI style-inheritance idiom used elsewhere in this file -
/// rather than the pane's previous `.text_xs()` (`rems(0.75)`, dependent on
/// `Window::rem_size()`) with no `.line_height()` at all (GPUI's own default,
/// `gpui::geometry::phi()` - the golden ratio, ~1.618× the font size, unrelated to this
/// font's real metrics). This closes the vertical half of a real, measured "scales weirdly"
/// bug - see [`TerminalPane::cell_size`]'s docs for the horizontal half and the fuller
/// before/after story.
const ROW_FONT_SIZE_PX: f32 = 12.0;
const ROW_LINE_HEIGHT_PX: f32 = 19.0;

/// Fallback monospace cell width, in pixels, used only until [`TerminalPane::cell_size`]'s
/// real measurement (`Window::text_system().advance`) succeeds at least once - i.e. before
/// the very first paint. Was, until this step, the *only* source for this value (a guess,
/// never actually measured against the real bundled IBM Plex Mono - see [`TerminalPane::
/// cell_size`]'s docs for the real, measured gap that left, and the fix).
const APPROX_CELL_WIDTH_PX: f32 = 7.0;

/// The pane root's own padding, applied on every side via `.p(px(PANE_PADDING_PX))` in
/// `render` (previously the GPUI-provided `.p_2()` shorthand - `rems(0.5)`, 8px at the
/// default 16px `Window::rem_size()` this app never overrides, per `gpui_macros::styles`'s
/// `box_style_suffixes`). Named as its own real `f32` constant, rather than left as `.p_2()`,
/// so [`TerminalPane::maybe_resize_pty`] can subtract the exact same padding it applies from
/// its own measured content-area bounds before converting to a grid size - see that method's
/// docs for the real padding-box-vs-content-box bug this fixes, and [`ROW_FONT_SIZE_PX`]'s
/// docs for the same "don't let a rendered value silently drift from an implicit default"
/// precedent this follows.
const PANE_PADDING_PX: f32 = 8.0;

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
    /// The real exit status of this pane's process, once it has exited - captured via a
    /// non-blocking `PtySession::try_wait` the moment the poll loop observes the output
    /// channel disconnect (see `Self::spawn_process`'s docs). `None` while a process is still
    /// running, before one has ever been spawned, or if a spawn attempt itself failed (see
    /// [`Self::spawn_error`] for that case instead - a process that never started has no real
    /// `ExitStatus` to report).
    exit_status: Option<ExitStatus>,
    /// The last time this pane's process is known to have produced real output, or - if it
    /// hasn't produced any yet - the moment it successfully started. `None` only before any
    /// process has ever successfully started. This is the raw signal
    /// `crate::status::derive_status`'s idle-time heuristic is built from (see that module's
    /// docs); it is intentionally *not* itself a `Status` - this pane has no notion of
    /// "session status", only "when did I last see this process do something".
    activity_at: Option<Instant>,
    /// `true` from the moment pty EOF is observed (the output channel disconnects) until the
    /// child's real exit status is either confirmed or given up on - see
    /// [`eof_poll_decision`]'s docs for the real bug this state exists to fix. While `true`,
    /// [`Self::session`] is deliberately *not* yet cleared: the process may genuinely still
    /// be alive (it closed its pty-attached stdio but hasn't exited), so
    /// [`Self::is_running`] correctly keeps reporting `true` during this window.
    eof_pending: bool,
    /// How many poll ticks [`Self::eof_pending`] has been `true` for - fed into
    /// [`eof_poll_decision`] each tick; reset whenever a fresh EOF is observed.
    eof_poll_ticks: u32,
    focus_handle: FocusHandle,
    /// This pane's own real, rendered content-area bounds - captured every frame via a
    /// measuring `canvas()` child in `render` (see that method's docs for why this exists
    /// instead of `window.viewport_size()`). `None` only before the very first paint.
    content_bounds: Option<Bounds<Pixels>>,
    /// The real, measured advance width of this pane's monospace font at
    /// [`ROW_FONT_SIZE_PX`] - see [`Self::cell_size`]'s docs. `None` until the first
    /// successful measurement (the loaded font/size never changes at runtime once it does,
    /// so this is cached rather than re-measured every render).
    cell_width_px: Option<Pixels>,
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
            exit_status: None,
            activity_at: None,
            eof_pending: false,
            eof_poll_ticks: 0,
            focus_handle: cx.focus_handle(),
            content_bounds: None,
            cell_width_px: None,
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

    /// `true` while a real child process is alive (spawned and not yet observed to have
    /// exited). The rail's real status derivation (`crate::status::derive_status`) uses this
    /// to distinguish a still-running session from one that has exited or never started.
    pub fn is_running(&self) -> bool {
        self.session.is_some()
    }

    /// How long it has been since this pane's process last produced real output (or since it
    /// started, if it hasn't produced any yet). `None` if no process is currently running -
    /// callers that need "no process" as a distinct case (rather than treating it as
    /// zero-idle) should check [`Self::is_running`] first; see `crate::rail`'s
    /// `process_signal` for exactly that.
    pub fn idle_duration(&self) -> Option<Duration> {
        if !self.is_running() {
            return None;
        }
        Some(
            self.activity_at
                .map(|at| at.elapsed())
                .unwrap_or(Duration::ZERO),
        )
    }

    /// The real exit status of this pane's process, once observed - see
    /// [`Self::exit_status`]'s field docs for exactly when this becomes `Some`.
    pub fn exit_status(&self) -> Option<&ExitStatus> {
        self.exit_status.as_ref()
    }

    /// The real error from the most recent failed spawn attempt, if any. A process that never
    /// started at all has no [`Self::exit_status`] to report, but is still a real failure the
    /// rail's status derivation should surface - see `crate::rail`'s `process_signal`.
    pub fn spawn_error(&self) -> Option<&str> {
        self.spawn_error.as_deref()
    }

    /// The real, resolved program name this pane was spawned with (e.g. `claude`, `codex`, or
    /// whatever `$SHELL` resolved to - `zsh`, `bash`, ...), used by `crate::root`'s Zone 2
    /// restyle for the CLI/terminal tab label and pane header
    /// (`design_handoff_jerry_ade/README.md`'s "the binary: `claude`, `codex`, `qwen`" and
    /// "`zsh` + worktree path" - real state, never the design's own sample strings). Falls
    /// back to the full path's own display form only in the (practically unreachable, since
    /// [`TerminalSpec::shell`]/`::command` always hand this a non-empty path) case
    /// `Path::file_name` returns `None`.
    pub fn program_label(&self) -> String {
        match self.spec.program.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => self.spec.program.display().to_string(),
        }
    }

    /// The real child process's OS pid, once spawned - `PtySession::process_id`, per
    /// `pty-core`'s own docs, `None` before a process exists or once the platform doesn't
    /// expose one. Backs the CLI pane header's real `pid 48213` (never a placeholder number).
    pub fn pid(&self) -> Option<u32> {
        self.session
            .as_ref()
            .and_then(|session| session.process_id())
    }

    /// Sends a real `Ctrl-C` (`0x03`) to the child process's pty, exactly as
    /// [`Self::handle_key_down`] would for an actual `Ctrl-C` keystroke - backs the surface
    /// footer's real `Interrupt \u{2303}C` action (`crate::root::AdeApp::interrupt_session`),
    /// not a simulated keypress. A no-op when no session is live (nothing to interrupt).
    pub fn interrupt(&mut self, _cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        // A failed write here is logged, not stored in `Self::spawn_error` - that field is
        // read by `crate::root::AdeApp::session_status` for an unrelated purpose (a process
        // that never *started* at all), and is only ever consulted while `!self.is_running()`
        // (see that method's docs). A live-enough-to-interrupt session is by definition
        // running, so this write failing could never actually reach that read today - but
        // storing it there anyway is the wrong field semantically, and would silently start
        // being read as "the process never started" the moment `is_running()` ever became
        // stale relative to a real write failure. `log::warn!` surfaces the real failure
        // without repurposing shared state for it.
        if let Err(err) = session.write_input(&[0x03]) {
            log::warn!("failed to send interrupt: {err}");
        }
    }

    /// The currently visible grid, as plain trimmed-right text lines (right-trimmed only -
    /// leading whitespace, e.g. an agent CLI's own indented menu, is preserved) - used by
    /// `crate::rail` to build the "question preview" the design calls "Jerry reading the tail
    /// of the agent's pty": a real read of this pane's own real terminal grid, not a
    /// reimplementation of pty reading (see `crate::terminal_grid::TerminalGrid::visible_rows`
    /// for the real grid this is built from).
    pub fn visible_text_lines(&self) -> Vec<String> {
        self.grid
            .visible_rows()
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
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
                    // A freshly started process hasn't produced any output yet, but it just
                    // demonstrably did something (started) - counting that as "activity now"
                    // is what keeps a session that's still spawning (or between its first two
                    // output chunks) from being immediately misread as long-idle by
                    // `crate::status::derive_status`.
                    this.activity_at = Some(std::time::Instant::now());
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
                    // Captured inside the `this.session.as_mut()` borrow below (so it can
                    // call the real, `&mut self` `PtySession::try_wait`) and only written
                    // back to `this.exit_status` once that borrow has ended.
                    let mut newly_exited: Option<ExitStatus> = None;

                    if this.eof_pending {
                        // EOF was already observed on a previous tick but the child's real
                        // exit status wasn't confirmed yet - see `eof_poll_decision`'s docs
                        // for the real race this branch exists to handle correctly (a child
                        // that closes its pty-attached stdio before actually exiting).
                        // `Self::session` is deliberately still `Some` here: the process may
                        // genuinely still be alive.
                        match this.session.as_mut() {
                            Some(session) => {
                                let wait_result = session.try_wait().map_err(|_| ());
                                match eof_poll_decision(wait_result, this.eof_poll_ticks) {
                                    Some(status) => {
                                        newly_exited = Some(status);
                                        process_ended = true;
                                    }
                                    None => this.eof_poll_ticks += 1,
                                }
                            }
                            None => process_ended = true, // defensive; shouldn't happen
                        }
                    } else if let Some(session) = this.session.as_mut() {
                        // Capped at `MAX_CHUNKS_PER_TICK`, not drained to empty: see that
                        // constant's docs for why an unbounded drain here is a real
                        // foreground-thread-starvation risk against a firehose child.
                        // Anything left in the channel just gets picked up next tick.
                        for _ in 0..MAX_CHUNKS_PER_TICK {
                            match session.output().try_recv() {
                                Ok(chunk) => {
                                    this.grid.append_bytes(&chunk);
                                    this.activity_at = Some(Instant::now());
                                    appended = true;
                                }
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Disconnected) => {
                                    this.eof_pending = true;
                                    let wait_result = session.try_wait().map_err(|_| ());
                                    match eof_poll_decision(wait_result, 0) {
                                        Some(status) => {
                                            newly_exited = Some(status);
                                            process_ended = true;
                                        }
                                        None => this.eof_poll_ticks = 1,
                                    }
                                    break;
                                }
                            }
                        }
                    }

                    if let Some(status) = newly_exited {
                        this.exit_status = Some(status);
                    }

                    if process_ended {
                        this.session = None;
                        this.eof_pending = false;
                        this.eof_poll_ticks = 0;
                        this.grid.mark_ended();
                        appended = true;
                    }

                    if appended {
                        cx.notify();
                    }

                    // Keep polling as long as there's a live session, or we're still waiting
                    // on a final exit status after EOF (the latter matters because
                    // `this.session` can be `Some` while `eof_pending` is `true`, so the two
                    // conditions aren't redundant).
                    this.session.is_some() || this.eof_pending
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

    /// This pane's real, measured monospace cell size - `(the real advance width of 'm' in
    /// the actual bundled IBM Plex Mono at `ROW_FONT_SIZE_PX`, `ROW_LINE_HEIGHT_PX`)`. The
    /// width comes from GPUI's own real font-metrics API, `Window::text_system().advance`
    /// (verified against `vendor/zed/crates/terminal_view/src/terminal_element.rs:1284`,
    /// which computes its own terminal's `cell_width` the exact same way: `text_system.
    /// advance(font_id, font_pixels, 'm').unwrap().width`) - not a guess. The height is
    /// [`ROW_LINE_HEIGHT_PX`] directly: [`render_row`] sets that as this pane's own explicit,
    /// controlled `.line_height()`, so there is nothing to *measure* for height - it's a
    /// known fact by construction, not a second approximation.
    ///
    /// Cached in [`Self::cell_width_px`] after the first successful measurement (resolving a
    /// font and querying its metrics has some real cost, and the loaded font/size never
    /// changes at runtime), falling back to [`APPROX_CELL_WIDTH_PX`] only on an outright
    /// measurement failure (e.g. the font somehow isn't resolvable) - a defensive fallback,
    /// not the primary path.
    fn cell_size(&mut self, window: &Window) -> Size<Pixels> {
        let width = match self.cell_width_px {
            Some(width) => width,
            None => {
                let font_id = window
                    .text_system()
                    .resolve_font(&font(crate::theme::font::MONO));
                let measured = window
                    .text_system()
                    .advance(font_id, px(ROW_FONT_SIZE_PX), 'm')
                    .map(|advance| advance.width)
                    .ok()
                    .filter(|width| *width > px(0.0));
                let width = measured.unwrap_or(px(APPROX_CELL_WIDTH_PX));
                // `debug!`, not `info!` - useful when actually diagnosing a sizing bug (see
                // this method's own docs for the real one this replaced), silent by default
                // (`main.rs`'s `env_logger` filter defaults to `info`).
                log::debug!(
                    "terminal_pane: measured real cell width = {width:?} (font-metrics lookup \
                     succeeded: {}; {APPROX_CELL_WIDTH_PX}px fallback used otherwise)",
                    measured.is_some()
                );
                self.cell_width_px = Some(width);
                width
            }
        };
        gpui::size(width, px(ROW_LINE_HEIGHT_PX))
    }

    /// Recomputes a real `(rows, cols)` from this pane's own real content-area bounds (see
    /// [`Self::content_bounds`]'s docs) and its own real, measured cell size (see
    /// [`Self::cell_size`]), then applies it via [`Self::resize_to`]. Called from `render` so
    /// it naturally re-runs whenever the pane's own size changes - not just whenever the
    /// *window's* size changes, since Phase A's three-zone shell means those are no longer
    /// the same thing (see [`size_to_grid`]'s docs for the real bug this distinction fixes).
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
    ///
    /// [`Self::content_bounds`] itself is the *padding box*, not the content box glyphs
    /// actually render into: `render`'s measuring `canvas()` is `.absolute().size_full()`
    /// inside the pane root, and that root also carries [`PANE_PADDING_PX`] of real padding -
    /// an absolutely positioned, size-full child sizes itself against its positioned ancestor's
    /// padding box (padding included), while the row `div`s (normal flow, not absolutely
    /// positioned) are laid out inside the *content* box, inset by that same padding. Real,
    /// measured proof: an 844x713px pane at the real 7.2x19.0px cell size computed 117
    /// cols/37 rows from the raw padding-box measurement, but only ~115 cols/~36 rows
    /// actually fit once [`PANE_PADDING_PX`] (applied twice - once per side) is subtracted -
    /// the extra column/row's glyphs painted straight through the pane's own
    /// `overflow_hidden()` clip edge. [`size_to_grid`]'s own `.max(20)`/`.max(10)` floors
    /// already handle the (here, practically unreachable) case of the padding subtraction
    /// driving a dimension negative, so no separate clamp is needed here.
    fn maybe_resize_pty(&mut self, window: &Window) {
        let raw_size = self
            .content_bounds
            .map(|bounds| bounds.size)
            .unwrap_or_else(|| window.viewport_size());
        let size = content_size_from_padding_box(raw_size);
        let cell_size = self.cell_size(window);
        let (rows, cols) = size_to_grid(size, cell_size);
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

/// Converts a pixel-space size into a `(rows, cols)` terminal grid size, given the real,
/// caller-measured `cell_size` a single monospace cell renders at (see [`TerminalPane::
/// cell_size`]). Deliberately a pure function of two [`Size<Pixels>`] values, independent of
/// `Window` or this pane's own state, so it's directly unit-testable (see the tests below)
/// rather than only exercisable through a real GPUI window.
///
/// Two real, independently-confirmed bugs lived here across two phases, both in *what this
/// function was called with*, never in the arithmetic itself:
///
/// - **Phase A: the wrong size.** `TerminalPane::maybe_resize_pty` used to pass the *whole
///   window's* `viewport_size()`, unconditionally. Phase A's three-zone shell added a 276px
///   rail, a 320px panel, and ~64px of title/status-bar chrome around the centre pane, none
///   of which this pane's own content occupies - at a 1440px-wide window that computed
///   roughly 205 columns even though the real visible terminal pane is only around 820-840px
///   wide. Fixed by calling this with the pane's own real, measured content-area size
///   (`TerminalPane::content_bounds`) instead of the window's.
/// - **Phase C: the wrong cell size.** Even after that fix, `cell_size` itself was
///   [`APPROX_CELL_WIDTH_PX`]/an `APPROX_CELL_HEIGHT_PX` of `16.0` - both guessed, never
///   measured against the real bundled IBM Plex Mono or this pane's real rendered line
///   height. The height guess was the bigger of the two: this pane's rows had no explicit
///   `.line_height()` at all before this phase, so GPUI's own default (`gpui::geometry::
///   phi()`, the golden ratio, ~1.618× the 12px font size ≈ 19.4px) was what *actually*
///   rendered - the `16.0` guess fed to this function was ~21% short of that, so
///   `maybe_resize_pty` always asked for more rows than could actually fit in the pane's real
///   height, and the bottom rows silently rendered past the pane's `overflow_hidden()` clip.
///   Fixed by making the line height an explicit, controlled fact
///   ([`ROW_LINE_HEIGHT_PX`], set on every row by [`render_row`]) instead of an unrelated
///   implicit default, and by measuring the real cell width via GPUI's own font-metrics API
///   (`Window::text_system().advance`) instead of guessing it - see [`TerminalPane::
///   cell_size`]'s docs, including this step's own before/after measurement.
fn size_to_grid(size: Size<Pixels>, cell_size: Size<Pixels>) -> (u16, u16) {
    let cols = ((size.width.as_f32() / cell_size.width.as_f32()) as u16).max(20);
    let rows = ((size.height.as_f32() / cell_size.height.as_f32()) as u16).max(10);
    (rows, cols)
}

/// Converts [`TerminalPane::content_bounds`]'s raw *padding-box* measurement into the real
/// content-box size glyphs actually render into, by subtracting [`PANE_PADDING_PX`] from
/// each side of both dimensions - see [`TerminalPane::maybe_resize_pty`]'s docs for the real,
/// measured padding-box-vs-content-box bug this fixes. A pure function of one `Size<Pixels>`,
/// factored out of `maybe_resize_pty` so this step's own real before/after numbers (a real
/// 844x713px pane) are directly unit-testable below without a live GPUI window.
fn content_size_from_padding_box(padding_box: Size<Pixels>) -> Size<Pixels> {
    let padding = px(PANE_PADDING_PX * 2.0);
    gpui::size(padding_box.width - padding, padding_box.height - padding)
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
    // Never forward a platform (⌘ on macOS, Super/Meta on Linux)-modified keystroke as
    // literal pty input. Two real reasons, not just one: it would type garbage into the
    // child process, and - since `handle_key_down` calls `cx.stop_propagation()` after any
    // successful write - it would swallow an app-level shortcut (e.g. the rail's ⌘N "new
    // session", see `crate::root::NewSession`) before it ever reaches its `KeyBinding`,
    // simply because a terminal tab happened to have focus. This must run *before* the
    // fallthrough `key_char` branch below, which otherwise returns a character for any
    // keystroke that has one - including platform-modified ones. Mirrors the same guard
    // `crate::root::AdeApp::handle_filter_key_down` already applies to its own text field.
    if keystroke.modifiers.platform {
        return None;
    }

    // Ctrl+<letter> control codes (Ctrl-A through Ctrl-Z), e.g. Ctrl-C -> 0x03 (SIGINT at
    // the line discipline), Ctrl-D -> 0x04 (EOF). Computed rather than hardcoded per-key:
    // this is the standard terminal mapping (`letter.to_ascii_uppercase() as u8 & 0x1f`).
    // (`modifiers.platform` is already excluded by the early return above.)
    if keystroke.modifiers.control && !keystroke.modifiers.alt {
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
            // `.p(px(PANE_PADDING_PX))`, not the equivalent `.p_2()` shorthand - see
            // `PANE_PADDING_PX`'s docs for why this needs to be the exact same real value
            // `Self::maybe_resize_pty` subtracts from its own measured bounds.
            .p(px(PANE_PADDING_PX))
            // The real bundled IBM Plex Mono (`crate::fonts`), not a generic "monospace"
            // alias - the terminal is this app's most prominent monospace surface, so it's
            // the clearest place to prove the bundled font is actually rendering.
            .font(font(crate::theme::font::MONO))
            // Explicit, not `.text_xs()` - `design_handoff_jerry_ade/README.md`'s "lines at
            // 12px/19 mono" exactly, and the same real values `Self::cell_size`'s real
            // measurement/`size_to_grid`'s real row math are built on. See `ROW_FONT_SIZE_PX`/
            // `ROW_LINE_HEIGHT_PX`'s docs for why leaving either of these implicit was a real,
            // measured bug.
            .text_size(px(ROW_FONT_SIZE_PX))
            .line_height(px(ROW_LINE_HEIGHT_PX))
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

    /// A representative real cell size, matching what `TerminalPane::cell_size` measures in
    /// practice (real IBM Plex Mono advance width at 12px is close to `APPROX_CELL_WIDTH_PX`,
    /// per this step's own before/after measurement - see `size_to_grid`'s docs) - used by
    /// tests below that only care about `size_to_grid`'s arithmetic, not about the exact cell
    /// size.
    fn test_cell_size() -> Size<Pixels> {
        size(px(APPROX_CELL_WIDTH_PX), px(ROW_LINE_HEIGHT_PX))
    }

    #[test]
    fn size_to_grid_derives_columns_and_rows_from_the_given_size_not_a_fixed_constant() {
        // A plausible centre-pane content width once Phase A's shell chrome (a 276px rail
        // plus a 320px panel plus borders) is subtracted from a 1440px window - roughly
        // 820px, not the full 1440.
        let (rows, cols) = size_to_grid(size(px(820.0), px(800.0)), test_cell_size());
        assert_eq!(cols, (820.0 / APPROX_CELL_WIDTH_PX) as u16);
        assert_eq!(rows, (800.0 / ROW_LINE_HEIGHT_PX) as u16);
    }

    #[test]
    fn size_to_grid_enforces_a_minimum_row_and_column_count() {
        let (rows, cols) = size_to_grid(size(px(10.0), px(10.0)), test_cell_size());
        assert_eq!(cols, 20);
        assert_eq!(rows, 10);
    }

    #[test]
    fn size_to_grid_from_a_real_pane_width_is_plausible_not_the_full_window_derived_count() {
        // Regression guard documenting the actual magnitude of the Phase-A bug: deriving
        // columns from the *whole* 1440px window (ignoring the 276+320px of shell chrome
        // either side) computed roughly 205 columns; the real, visible centre-pane width is
        // roughly 820-840px, around 118-120 columns. Both numbers are "correct" outputs of
        // `size_to_grid` for their respective inputs - the bug was `maybe_resize_pty`
        // feeding it the wrong one. This test pins the two magnitudes apart so a future
        // regression back to `window.viewport_size()` would be caught by inspection here.
        let (_rows, whole_window_cols) =
            size_to_grid(size(px(1440.0), px(928.0)), test_cell_size());
        let (_rows, real_pane_cols) = size_to_grid(size(px(828.0), px(800.0)), test_cell_size());
        assert!(
            whole_window_cols > real_pane_cols * 3 / 2,
            "expected the whole-window column count ({whole_window_cols}) to be \
             substantially larger than the real-pane-width column count ({real_pane_cols}) \
             - if they're close, something about the approximation changed"
        );
        assert!(real_pane_cols < 130, "got {real_pane_cols}");
    }

    #[test]
    fn size_to_grid_uses_the_real_given_cell_size_not_an_internal_constant() {
        // Regression guard for the Phase-C bug: before this phase, `size_to_grid` computed
        // columns/rows from its own hardcoded `APPROX_CELL_WIDTH_PX`/`APPROX_CELL_HEIGHT_PX`
        // constants, ignoring whatever the pane's *real* rendered cell size actually was. It
        // now takes `cell_size` as a real parameter - a bigger cell must produce fewer
        // columns/rows for the same pixel area, and the ratio must match exactly (not just
        // "fewer somehow").
        let small_cells = size_to_grid(size(px(800.0), px(760.0)), size(px(8.0), px(19.0)));
        let big_cells = size_to_grid(size(px(800.0), px(760.0)), size(px(16.0), px(38.0)));
        assert_eq!(small_cells, (40, 100));
        assert_eq!(
            big_cells,
            (small_cells.0 / 2, small_cells.1 / 2),
            "doubling the cell size must exactly halve both dimensions"
        );
    }

    #[test]
    fn size_to_grid_documents_the_real_before_after_column_count_at_a_typical_pane_width() {
        // Real before/after numbers for this step's own performance/scaling report, pinned as
        // a test so they can't silently drift out of sync with the actual constants: at a
        // plausible real centre-pane width (828px, per the test above) and a typical pty
        // panel height (~800px), how many columns/rows the *old* guessed cell size
        // (7.0 x 16.0) computed versus the *new* real line-height-corrected one (7.0 x 19.0).
        // Width is unchanged in this comparison (this test isolates the height/line-height
        // half of the Phase-C bug; the width half is that `cell_size`'s width now comes from
        // a real GPUI font-metrics measurement instead of a guess, which this pure function
        // can't itself demonstrate - see `TerminalPane::cell_size`'s own docs for that half).
        let old_guess = size(px(APPROX_CELL_WIDTH_PX), px(16.0));
        let new_real = size(px(APPROX_CELL_WIDTH_PX), px(ROW_LINE_HEIGHT_PX));
        let pane = size(px(828.0), px(800.0));

        let (old_rows, _) = size_to_grid(pane, old_guess);
        let (new_rows, _) = size_to_grid(pane, new_real);

        // The old, too-short line-height guess asked the pty for more rows than the pane
        // could actually show without clipping (`800.0 / 16.0 = 50` rows requested, but at
        // the real ~19px line height only `800.0 / 19.0 = 42` actually fit) - a real ~19%
        // over-request, not a rounding artifact.
        assert_eq!(old_rows, 50);
        assert_eq!(new_rows, 42);
        assert!(
            old_rows > new_rows,
            "the old guess must over-request rows relative to the real, corrected line height"
        );
    }

    #[test]
    fn content_size_from_padding_box_subtracts_the_real_pane_padding_from_both_sides() {
        // `PANE_PADDING_PX` (8.0) is applied on every side, so both dimensions must shrink
        // by twice that, not once.
        let padding_box = size(px(844.0), px(713.0));
        let content = content_size_from_padding_box(padding_box);
        assert_eq!(content.width, px(844.0 - 16.0));
        assert_eq!(content.height, px(713.0 - 16.0));
    }

    #[test]
    fn padding_box_measurement_over_requests_columns_and_rows_relative_to_the_real_content_box() {
        // The real, checker-reproduced padding-box-vs-content-box bug this test pins: a real
        // measured pane of 844x713px with a real measured cell size of 7.2x19.0px. Before the
        // fix, `maybe_resize_pty` fed the *raw* padding-box measurement straight into
        // `size_to_grid`, computing 117 cols/37 rows - one more of each than the pane's real
        // content box (844-16=828px wide, 713-16=697px tall) actually fits (115 cols/36
        // rows), so the last column/row's glyphs painted through the pane's own
        // `overflow_hidden()` clip edge.
        let padding_box = size(px(844.0), px(713.0));
        let real_cell_size = size(px(7.2), px(19.0));

        let (over_requested_rows, over_requested_cols) = size_to_grid(padding_box, real_cell_size);
        assert_eq!((over_requested_rows, over_requested_cols), (37, 117));

        let content_box = content_size_from_padding_box(padding_box);
        let (fitting_rows, fitting_cols) = size_to_grid(content_box, real_cell_size);
        assert_eq!((fitting_rows, fitting_cols), (36, 115));

        assert!(
            over_requested_cols > fitting_cols && over_requested_rows > fitting_rows,
            "the raw padding-box measurement must over-request relative to the real content box"
        );
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

#[cfg(test)]
mod eof_poll_tests {
    use super::*;

    #[test]
    fn resolves_immediately_when_try_wait_already_has_a_status() {
        let status = ExitStatus::with_exit_code(7);
        match eof_poll_decision(Ok(Some(status)), 0) {
            Some(resolved) => assert_eq!(resolved.exit_code(), 7),
            None => panic!("expected an immediate resolution from a ready try_wait result"),
        }
    }

    #[test]
    fn keeps_waiting_while_try_wait_has_no_answer_and_the_tick_cap_is_not_reached() {
        assert!(eof_poll_decision(Ok(None), 0).is_none());
        assert!(eof_poll_decision(Ok(None), MAX_EOF_POLL_TICKS - 1).is_none());
        assert!(
            eof_poll_decision(Err(()), MAX_EOF_POLL_TICKS - 1).is_none(),
            "a transient try_wait error must also be retried, not treated as final"
        );
    }

    #[test]
    fn gives_up_at_the_tick_cap_with_a_synthetic_failed_status_not_silence() {
        // The exact bug this whole decomposition exists to prevent: giving up must never
        // look like "no exit status at all" (which `crate::status::derive_status` would
        // read as `Status::Idle`) - it must resolve to a real, if synthetic, failed status.
        match eof_poll_decision(Ok(None), MAX_EOF_POLL_TICKS) {
            Some(status) => assert!(
                !status.success(),
                "giving up must never be reported as a successful exit"
            ),
            None => panic!("expected the tick cap to force a resolution"),
        }
    }

    /// Real, empirical proof (no GPUI needed) that the race `eof_poll_decision` exists to
    /// handle is genuine: a real child spawned via `pty_core::spawn` that closes its own
    /// pty-attached stdio *before* actually exiting causes the output channel to disconnect
    /// (real EOF) while the process is still alive - a single non-blocking `try_wait` at
    /// that exact moment legitimately observes nothing, and only a later retry observes the
    /// real exit status. This is the same repro the checker used to find the original bug.
    #[test]
    fn real_process_closing_pty_fds_before_exiting_is_not_yet_reaped_at_eof() {
        let mut session = pty_core::spawn(
            pty_core::SpawnOptions::new("sh")
                .arg("-c")
                .arg("exec 0<&- 1>&- 2>&-; sleep 1; exit 7"),
        )
        .expect("spawning the shell should succeed");

        // Drain until the output channel disconnects - exactly the `TryRecvError::
        // Disconnected` signal `TerminalPane`'s poll loop reacts to.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for the pty output channel to disconnect (EOF)");
            }
            if session.output().recv_timeout(remaining).is_err() {
                break; // disconnected
            }
        }

        // At the moment of EOF the process has not exited yet (it's mid-`sleep 1`) - a
        // single `try_wait` here legitimately observes nothing. If this assertion itself
        // fails, the test's timing assumption is wrong, not the fix under test.
        let immediately_after_eof = session
            .try_wait()
            .expect("try_wait should not error for a live child");
        assert!(
            immediately_after_eof.is_none(),
            "expected the process to still be alive (sleeping) right after its pty fds \
             closed - a single try_wait must not yet observe an exit"
        );

        // The bounded retry loop `eof_poll_decision` drives in real usage: keep checking
        // until the real exit status becomes observable.
        let mut observed = None;
        let poll_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < poll_deadline {
            if let Some(status) = session
                .try_wait()
                .expect("try_wait should not error while polling")
            {
                observed = Some(status);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let status =
            observed.expect("the process should eventually be reaped with a real exit status");
        assert!(!status.success());
        assert_eq!(status.exit_code(), 7);
    }
}

#[cfg(test)]
mod keystroke_tests {
    use super::*;
    use gpui::Modifiers;

    fn keystroke(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: key.to_string(),
            key_char: Some(key.to_string()),
            modifiers,
        }
    }

    #[test]
    fn platform_modified_keystrokes_are_never_forwarded_as_literal_pty_input() {
        // Regression test for the ⌘N-swallowed-by-a-focused-terminal bug: before the fix,
        // this fell through to the `key_char` branch and returned `Some(b"n")`, which
        // `handle_key_down` then wrote to the pty *and* called `stop_propagation()` on -
        // silently eating the app-level ⌘N shortcut and typing a stray "n" into the agent.
        let modifiers = Modifiers {
            platform: true,
            ..Default::default()
        };
        let ks = keystroke("n", modifiers);

        assert_eq!(
            keystroke_to_bytes(&ks),
            None,
            "a platform-modified keystroke must never be forwarded as pty input"
        );
    }

    #[test]
    fn an_unmodified_letter_is_still_forwarded_normally() {
        let ks = keystroke("n", Modifiers::default());
        assert_eq!(keystroke_to_bytes(&ks), Some(b"n".to_vec()));
    }
}
