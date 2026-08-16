//! A single terminal-backed pane: a real process (a plain shell, or an agent CLI like
//! `claude`) spawned via `pty-core`, streamed into a real `alacritty_terminal`-backed
//! [`crate::terminal::grid::TerminalGrid`] and rendered as a genuine cursor-addressed
//! terminal grid, not a scrolling plain-text log (see `crate::terminal::grid`'s module docs
//! for why that distinction matters for agent CLIs). What gets spawned is described by
//! [`TerminalSpec`] - `TerminalPane` itself has no notion of "shell" vs. "agent CLI". The
//! agent/tab bookkeeping that decides which spec to use, and that owns more than one
//! `TerminalPane` as tabs, lives in `crate::work_surface::agents`/`crate::root`, one layer up.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use gpui::{
    canvas, div, font, prelude::*, px, rgb, BorderStyle, Bounds, ClickEvent, Context, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyDownEvent, Keystroke, Modifiers, Pixels,
    ScrollWheelEvent, Size, Task, Window,
};
use pty_core::{ExitStatus, PtyError, PtySession, SpawnOptions};

use crate::root::scrollbar::{self, ScrollableHandle};
use crate::root::widgets::text_tooltip;
use crate::terminal::grid::{
    CellPosition, CellSide, CellWidth, GridCell, ScrollAmount, TerminalGrid, TerminalPalette,
};
use crate::terminal::links::{self as terminal_links, LinkMatch};
use crate::terminal::osc::Progress;
use crate::theme;

/// How often the poll task of the *globally active* agent's pane (see
/// [`TerminalPane::set_foreground`]; every other pane uses [`BACKGROUND_POLL_INTERVAL`]) wakes
/// up to drain any pty output that has arrived and, if there was any, re-render.
const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// [`POLL_INTERVAL`]'s counterpart for a pane whose agent is *not* the globally active tab
/// (see [`TerminalPane::set_foreground`]) - nobody can see a background pane's output live, so
/// polling it twice per frame buys nothing. 33ms (the pre-tightening interval, ~30 drains/s)
/// keeps a background agent's status/activity signal fresh to within a frame or two while
/// capping how much foreground-thread work each background pane can generate.
const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_millis(33);

/// Defensive cap on how many output *bytes* a single poll tick will drain and decode on the
/// GPUI foreground thread. Without a cap, a firehose child (`yes`, a chatty build tool) could
/// hand the poll loop the full contents of `pty-core`'s bounded output channel (~1MB) to
/// decode in a single tick, on the same thread responsible for input handling and
/// re-rendering. Capping the per-tick budget spreads that cost across ticks instead - whatever
/// isn't drained is still sitting in the channel (pty-core's reader thread backpressures) and
/// gets picked up next tick.
const MAX_BYTES_PER_TICK: usize = 256 * 1024;

/// [`MAX_BYTES_PER_TICK`]'s counterpart for a background pane (see
/// [`BACKGROUND_POLL_INTERVAL`]'s docs for the measured multi-pane regression this split
/// fixes). 32KiB per 33ms tick caps a background pane's *delivered* throughput at ~1MB/s -
/// deliberately about what the pre-tightening cadence delivered (the old 64-chunk cap measured
/// ~0.8MB/s against a real firehose), so a wall of background agents can never generate more
/// aggregate foreground decode work than the app handled before the tightening. The child
/// isn't harmed: `pty-core`'s bounded channel backpressures it, exactly as it did for every
/// pane pre-tightening, and the pane returns to the full [`MAX_BYTES_PER_TICK`] budget the
/// moment its tab becomes active again.
const BACKGROUND_MAX_BYTES_PER_TICK: usize = 32 * 1024;

/// The poll cadence - (sleep interval, per-tick drain byte budget) - for one tick of
/// [`TerminalPane::spawn_process`]'s loop, given whether this pane's agent is the globally
/// active tab and whether pty EOF is already pending.
fn tick_cadence(is_foreground: bool, eof_pending: bool) -> (Duration, usize) {
    if is_foreground || eof_pending {
        (POLL_INTERVAL, MAX_BYTES_PER_TICK)
    } else {
        (BACKGROUND_POLL_INTERVAL, BACKGROUND_MAX_BYTES_PER_TICK)
    }
}

/// Initial pty size used for the spawned shell, before the first real resize (see
/// `maybe_resize_pty`) has a chance to run during the first render.
const TERMINAL_ROWS: u16 = 48;
const TERMINAL_COLS: u16 = 160;

/// How many [`POLL_INTERVAL`] ticks (~10s total) [`TerminalPane`]'s poll loop keeps retrying
/// `PtySession::try_wait` after observing pty EOF before giving up - see
/// [`eof_poll_decision`]'s docs for the race this bounds.
const MAX_EOF_POLL_TICKS: u32 = (10_000 / POLL_INTERVAL.as_millis()) as u32;

/// Decides what a poll tick should do once pty EOF has been observed (the output channel's
/// `TryRecvError::Disconnected`) but the child's exit status hasn't been confirmed yet, given
/// this tick's own non-blocking `PtySession::try_wait` result.
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

/// The terminal body's default/fallback font size and line height -
/// `design_handoff_jerry_ade/README.md`'s Surface A/B body spec, "lines at 12px/19 mono". Set
/// explicitly (`.text_size()`/`.line_height()`) on the pane root by [`TerminalPane`]'s
/// `Render` impl, not per-row - rows inherit both from the root. Explicit rather than the
/// GPUI-default `.text_xs()` (no `.line_height()`, so GPUI's own default `phi()` - the golden
/// ratio, unrelated to this font's real metrics - was what rendered) because that mismatch
/// was a real, measured "scales weirdly" bug; see [`TerminalPane::cell_size`]'s docs for the
/// rest of that story.
const ROW_FONT_SIZE_PX: f32 = 12.0;
const ROW_LINE_HEIGHT_PX: f32 = 19.0;

/// Fallback monospace cell width, in pixels, used only until [`TerminalPane::cell_size`]'s
/// real measurement (`Window::text_system().advance`) succeeds at least once - i.e. before the
/// very first paint.
const APPROX_CELL_WIDTH_PX: f32 = 7.0;

/// How many grid lines one detent ("notch") of a real mouse wheel is worth, used only to convert
/// [`TerminalPane::handle_scroll_wheel`]'s line count back into wheel gestures for
/// [`TerminalPane::forward_scroll_as_page_keys`] (GitHub issue #368).
const WHEEL_LINES_PER_NOTCH: f32 = 3.0;

/// The pane root's own padding, applied on every side via `.p(px(PANE_PADDING_PX))` in
/// `render`. Named as its own constant rather than left as the equivalent `.p_2()` shorthand
/// so [`TerminalPane::maybe_resize_pty`] can subtract the exact same padding it applies from
/// its own measured content-area bounds before converting to a grid size - see that method's
/// docs for the padding-box-vs-content-box bug this fixes.
pub(crate) const PANE_PADDING_PX: f32 = 8.0;

/// What a [`TerminalPane`] should spawn: a program, its arguments, and the working directory
/// to spawn it in. Generalizes "always spawn `$SHELL`" so the same pane implementation can
/// host a plain shell *or* an agent CLI (`claude`, `codex`, ...) - see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Extra environment variables set on the spawned child, *on top of* the environment Jerry
    /// itself inherited (`portable_pty`'s `CommandBuilder::env` adds to the inherited
    /// environment rather than replacing it, which is what makes this safe to use for a couple
    /// of variables without having to reconstruct a whole environment).
    pub env: Vec<(String, String)>,
}

impl TerminalSpec {
    /// The user's shell, no extra arguments - GitHub issue #50 (real, `/bin/bash` unconditional
    /// even on Windows): `$SHELL` is a real Unix environment-variable convention that Windows
    /// itself never sets, so the fallback needs its own real Windows equivalent rather than the
    /// same Unix path unconditionally, which is a real path (`/bin/bash`) that doesn't exist on
    /// Windows at all - every default-shell spawn there was trying, and failing, to launch it.
    pub fn shell(cwd: PathBuf, configured: Option<&str>) -> Self {
        Self {
            program: configured_shell_program(configured)
                .unwrap_or_else(Self::default_shell_program),
            args: Vec::new(),
            cwd,
            env: Vec::new(),
        }
    }

    /// Unix: `$SHELL`, falling back to `/bin/bash` if unset - unchanged from before issue #50.
    #[cfg(unix)]
    fn default_shell_program() -> PathBuf {
        shell_program_from_env(std::env::var_os("SHELL"), "/bin/bash")
    }

    /// Windows: `%COMSPEC%` - the real, documented Windows environment variable naming the
    /// user's configured command interpreter (almost always `C:\Windows\System32\cmd.exe`),
    /// the same real role `$SHELL` plays on Unix. Falls back to the bare name `cmd.exe` if
    /// unset (`cmd.exe` ships with every real Windows install and is always on `%PATH%`) rather
    /// than a hardcoded absolute path, mirroring the Unix fallback's own "a real, always-present
    /// binary" choice. A bare name, not an absolute path: `pty_core::spawn`'s own
    /// `CommandBuilder` already resolves a bare program name via `PATH` (see
    /// [`TerminalSpec::command`]'s own docs for the identical, already-relied-on mechanism), so
    /// this needs no separate resolution step of its own.
    #[cfg(windows)]
    fn default_shell_program() -> PathBuf {
        shell_program_from_env(std::env::var_os("COMSPEC"), "cmd.exe")
    }

    /// The real program a shell tab launches when the user has configured nothing (GitHub issue
    /// #213) - `$SHELL`'s value on this machine right now, `%COMSPEC%`'s on Windows, resolved by
    /// exactly the same code [`Self::shell`] falls back to. Exposed so the Settings row's
    /// placeholder can name the *actual* program an empty field means, rather than a generic
    /// `"$SHELL"` string that would be a second, drift-prone description of this decision.
    pub fn default_shell_program_display() -> String {
        Self::default_shell_program().display().to_string()
    }

    /// An arbitrary command. `program` may be a bare name (e.g. `"claude"`, no path
    /// separator): `pty-core`'s `spawn` resolves it via `PATH` through
    /// `portable_pty::CommandBuilder` the same way a shell would, so this doesn't need the
    /// caller to resolve an absolute path itself.
    pub fn command(program: impl Into<PathBuf>, args: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            program: program.into(),
            args,
            cwd,
            env: Vec::new(),
        }
    }

    /// [`Self::command`] plus extra environment variables for the child - see [`Self::env`].
    pub fn command_with_env(
        program: impl Into<PathBuf>,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            cwd,
            env,
        }
    }
}

/// The pure "real env var, or a real fallback" decision behind both
/// [`TerminalSpec::default_shell_program`] variants - not itself `#[cfg]`-gated, so it's directly
/// testable on any host regardless of which platform-specific env var name/fallback the caller
/// passes in.
fn shell_program_from_env(env_value: Option<std::ffi::OsString>, fallback: &str) -> PathBuf {
    env_value
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(fallback))
}

/// The user's configured shell (GitHub issue #213) as a real program to spawn, or `None` when
/// there genuinely isn't one - the pure half of [`TerminalSpec::shell`]'s decision, so both
/// branches are testable on any host without touching the process environment.
fn configured_shell_program(configured: Option<&str>) -> Option<PathBuf> {
    configured
        .map(str::trim)
        .filter(|program| !program.is_empty())
        .map(PathBuf::from)
}

/// GitHub issue #50's real regression guard: the Windows fallback must be a real Windows path
/// (`cmd.exe`), never the Unix `/bin/bash` [`TerminalSpec::shell`] used to fall back to
/// unconditionally - `$SHELL` is a Unix-only convention Windows never sets, so every Windows
/// session with no real override was actually trying to launch a Unix path that doesn't exist
/// there at all. `shell_program_from_env` itself isn't `#[cfg]`-gated, so both real platform
/// variants' own decisions are directly testable here regardless of which OS runs this test.
#[cfg(test)]
mod shell_program_tests {
    use super::*;

    #[test]
    fn a_real_env_value_is_used_verbatim() {
        assert_eq!(
            shell_program_from_env(Some(std::ffi::OsString::from("/usr/bin/zsh")), "/bin/bash"),
            PathBuf::from("/usr/bin/zsh")
        );
    }

    #[test]
    fn an_absent_env_value_falls_back() {
        assert_eq!(
            shell_program_from_env(None, "/bin/bash"),
            PathBuf::from("/bin/bash")
        );
    }

    #[test]
    fn a_configured_shell_replaces_the_environment_default() {
        let cwd = std::env::temp_dir();

        assert_eq!(
            TerminalSpec::shell(cwd.clone(), Some("fish")).program,
            PathBuf::from("fish"),
            "a bare name must reach the spawn as-is, for pty-core's own PATH resolution"
        );
        assert_eq!(
            TerminalSpec::shell(cwd.clone(), Some("/usr/local/bin/fish")).program,
            PathBuf::from("/usr/local/bin/fish"),
            "an absolute path must be used verbatim, not searched for on PATH"
        );
        assert!(
            TerminalSpec::shell(cwd.clone(), Some("fish"))
                .args
                .is_empty(),
            "a configured shell is launched exactly like the default one: no extra arguments"
        );
        assert_eq!(
            TerminalSpec::shell(cwd.clone(), Some("fish")).cwd,
            cwd,
            "choosing a shell must not change which directory it starts in"
        );
    }

    #[test]
    fn no_configured_shell_is_byte_for_byte_the_previous_os_default() {
        let cwd = std::env::temp_dir();
        let os_default = TerminalSpec::default_shell_program();

        assert_eq!(TerminalSpec::shell(cwd.clone(), None).program, os_default);
        assert_eq!(
            TerminalSpec::shell(cwd.clone(), Some("")).program,
            os_default,
            "an empty setting means 'use the system default', not a program with no name"
        );
        assert_eq!(
            TerminalSpec::shell(cwd, Some("   ")).program,
            os_default,
            "a whitespace-only setting must fall back too, never spawn a program named ' '"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_configured_shell_really_spawns_and_a_typo_really_fails() {
        let good = TerminalSpec::shell(std::env::temp_dir(), Some("sh"));
        let mut session = pty_core::spawn(
            pty_core::SpawnOptions::new(good.program.clone()).cwd(good.cwd.clone()),
        )
        .expect("a configured shell that really exists must spawn a real process");
        assert!(
            session.process_id().is_some(),
            "the configured shell must be a real, running child with a real pid"
        );
        session.shutdown().expect("reap the child");

        let bad = TerminalSpec::shell(
            std::env::temp_dir(),
            Some("definitely-not-a-real-shell-xyz"),
        );
        match pty_core::spawn(pty_core::SpawnOptions::new(bad.program).cwd(bad.cwd)) {
            Err(err) => assert!(
                matches!(err, pty_core::PtyError::Spawn(_)),
                "a misconfigured shell must fail with the same real, typed, reportable error \
                 any other missing program does, got {err:?}"
            ),
            Ok(_) => panic!("a shell that doesn't exist must not spawn"),
        }
    }

    #[test]
    fn a_configured_shell_is_trimmed_before_it_becomes_a_program() {
        assert_eq!(
            configured_shell_program(Some("  zsh  ")),
            Some(PathBuf::from("zsh"))
        );
        assert_eq!(configured_shell_program(None), None);
        assert_eq!(configured_shell_program(Some(" ")), None);
    }

    #[test]
    fn the_windows_fallback_is_a_real_windows_path_not_the_unix_one() {
        let windows_fallback = shell_program_from_env(None, "cmd.exe");
        assert_eq!(windows_fallback, PathBuf::from("cmd.exe"));
        assert_ne!(
            windows_fallback,
            PathBuf::from("/bin/bash"),
            "the Windows default shell must never be the Unix path - that real path does not \
             exist on Windows, which is the whole bug this issue reports"
        );
    }
}

/// Real GPUI scroll handle backing the terminal's own scrollback (GitHub issue #331) - the
/// [`ScrollableHandle`] adapter that lets [`TerminalPane`] reuse the exact same shared overlay
/// scrollbar every other scrollable region in this app uses
/// (`crate::root::scrollbar::render_vertical_scrollbar`, see that module's own docs), even
/// though the terminal's real scroll position is `alacritty_terminal::Term`'s line-based
/// `display_offset` (`TerminalGrid::scroll_offset`), not a GPUI-native scrollable div's pixel
/// offset.
#[derive(Clone)]
struct TerminalScrollHandle(Rc<RefCell<TerminalScrollState>>);

#[derive(Default)]
struct TerminalScrollState {
    viewport_bounds: Bounds<Pixels>,
    row_height: Pixels,
    history_len: usize,
    display_offset: usize,
    /// Set by [`ScrollableHandle::set_scroll_offset`] (a scrollbar click/drag); drained by
    /// [`TerminalPane::render`] at the top of the next render and turned into a real
    /// `TerminalGrid::set_scroll_offset` call.
    requested_display_offset: Option<usize>,
}

impl TerminalScrollHandle {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(TerminalScrollState::default())))
    }

    /// Pushes the grid's real current scroll state into the handle - called once per render,
    /// before the scrollbar itself is built, so [`ScrollableHandle`]'s geometry methods answer
    /// with this frame's real numbers rather than a stale previous frame's.
    fn sync(
        &self,
        viewport_bounds: Bounds<Pixels>,
        row_height: Pixels,
        history_len: usize,
        display_offset: usize,
    ) {
        let mut state = self.0.borrow_mut();
        state.viewport_bounds = viewport_bounds;
        state.row_height = row_height;
        state.history_len = history_len;
        state.display_offset = display_offset;
    }

    /// Drains a scrollbar-driven target display offset, if the user clicked or dragged the
    /// thumb since the last render.
    fn take_requested_display_offset(&self) -> Option<usize> {
        self.0.borrow_mut().requested_display_offset.take()
    }
}

impl ScrollableHandle for TerminalScrollHandle {
    /// Lines-from-top, not `TerminalGrid::scroll_offset`'s own live-relative convention: `0` at
    /// the *oldest* retained line (the top of the track) and [`TerminalScrollState::history_len`]
    /// at the live tail (the bottom of the track) - matching `gpui::ScrollHandle`'s own "offset
    /// grows toward the bottom of the content" convention, and what makes the scrollbar thumb
    /// sit at the *bottom* of the track while live-following, exactly like every real terminal
    /// emulator's own scrollbar.
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().viewport_bounds
    }

    fn max_scroll_offset(&self) -> gpui::Point<Pixels> {
        let state = self.0.borrow();
        gpui::point(
            px(0.0),
            px(state.row_height.as_f32() * state.history_len as f32),
        )
    }

    fn scroll_offset(&self) -> gpui::Point<Pixels> {
        let state = self.0.borrow();
        let lines_from_top = state.history_len.saturating_sub(state.display_offset) as f32;
        gpui::point(px(0.0), px(-(state.row_height.as_f32() * lines_from_top)))
    }

    fn set_scroll_offset(&self, offset: gpui::Point<Pixels>) {
        let mut state = self.0.borrow_mut();
        let row_height = state.row_height.as_f32();
        if row_height <= 0.0 {
            return;
        }
        let max_px = row_height * state.history_len as f32;
        let scrolled_px = (-offset.y.as_f32()).clamp(0.0, max_px);
        let lines_from_top = (scrolled_px / row_height).round() as usize;
        state.requested_display_offset = Some(state.history_len.saturating_sub(lines_from_top));
    }
}

/// An event [`TerminalPane`] emits (`cx.emit`) for its owner to react to - the same
/// `EventEmitter`/`cx.subscribe_in` pattern `vendor/zed/crates/terminal/src/terminal.rs`'s own
/// `Event::Open(MaybeNavigationTarget)` uses for the same "a click resolved to a navigation
/// target" case (`vendor/zed/crates/terminal/src/terminal.rs:1823`). `TerminalPane` itself has
/// no notion of tabs/file-opening (see the module docs), so it can only announce "a link was
/// clicked"; `crate::work_surface::agents::Agents::spawn` subscribes and turns this into a
/// `crate::root::AdeApp::open_terminal_link` call, since that layer owns tab state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalPaneEvent {
    /// A `mod`-held click on a detected link (`crate::terminal::links::find_links`) inside this
    /// pane's rendered grid - `path` is already resolved against this pane's own
    /// `TerminalSpec::cwd` (see [`render_link_span`]), never a bare, unresolved string.
    OpenPath { path: PathBuf, line: Option<u32> },
}

pub struct TerminalPane {
    spec: TerminalSpec,
    grid: TerminalGrid,
    session: Option<PtySession>,
    spawn_error: Option<String>,
    /// The process's exit status, once it has exited - captured via a non-blocking
    /// `PtySession::try_wait` the moment the poll loop notices the process ended: on unix, when
    /// the output channel disconnects (real pty EOF); on Windows, from an independent
    /// `try_wait` poll every tick, since pty EOF can't be relied on there (see the module docs'
    /// "Windows: process-exit detection can't rely on pty EOF" section). `None` while running,
    /// before ever spawned, or if a spawn attempt itself failed (see [`Self::spawn_error`] for
    /// that case - a process that never started has no `ExitStatus` to report).
    exit_status: Option<ExitStatus>,
    /// The last time this pane's process is known to have produced output, or - if it hasn't
    /// produced any yet - the moment it started. `None` only before any process has ever
    /// started. The raw signal `crate::rail::status::derive_status`'s idle-time heuristic is built
    /// from; intentionally not itself a `Status` - this pane only knows "when did I last see
    /// this process do something".
    activity_at: Option<Instant>,
    /// When this pane's process last fired an OSC 9 / OSC 777 desktop notification that the
    /// human has not yet answered, or `None` if it never has or the human has since answered
    /// (GitHub issue #239).
    attention_ping_at: Option<Instant>,
    /// `true` from the moment pty EOF is observed until the child's exit status is either
    /// confirmed or given up on - see [`eof_poll_decision`]'s docs for the bug this state
    /// exists to fix. While `true`, [`Self::session`] is deliberately not yet cleared: the
    /// process may genuinely still be alive.
    eof_pending: bool,
    /// How many poll ticks [`Self::eof_pending`] has been `true` for - fed into
    /// [`eof_poll_decision`] each tick; reset whenever a fresh EOF is observed.
    eof_poll_ticks: u32,
    focus_handle: FocusHandle,
    /// This pane's rendered content-area bounds - captured every frame via a measuring
    /// `canvas()` child in `render` (see that method's docs for why this exists instead of
    /// `window.viewport_size()`). `None` only before the first paint.
    content_bounds: Option<Bounds<Pixels>>,
    /// This pane's current font size in pixels - the persisted `appearance.terminal_font_size`
    /// setting, pushed in at construction ([`Self::new`]) and again on every edit
    /// (`crate::root::AdeApp`'s Settings › Appearance page, via [`Self::set_font_size`]).
    /// [`Self::line_height_px`] is always derived from this, keeping the design's `12px/19`
    /// ratio rather than drifting independently.
    font_size_px: f32,
    /// The measured advance width of this pane's monospace font at [`Self::font_size_px`] -
    /// see [`Self::cell_size`]'s docs. `None` until the first successful measurement for the
    /// current [`Self::font_size_px`] - [`Self::set_font_size`] resets this to `None` on a
    /// change, since a width cached at the old size would otherwise keep being used for
    /// grid/pty sizing.
    cell_width_px: Option<Pixels>,
    /// Tracks which `(rows, cols)` the grid and the child pty are each actually in sync with -
    /// see [`ResizeLatch`]'s docs for the bug this decomposition exists to prevent.
    resize_latch: ResizeLatch,
    /// Owns the in-flight "spawn the process, then poll its output" task. Dropping/replacing
    /// this cancels whatever the previous task was doing, stopping an old session's poll loop
    /// from racing a new one over the same struct fields.
    _task: Option<Task<()>>,
    /// Whether this pane's agent is the *globally active* tab
    /// (`crate::work_surface::agents::Agents::active`). Drives [`tick_cadence`]: only the active
    /// agent's pane gets the frame-accurate [`POLL_INTERVAL`]/[`MAX_BYTES_PER_TICK`]
    /// cadence; every other pane polls at the coarser background cadence.
    is_foreground: bool,
    /// `true` between a real left mouse-down inside the grid and the matching mouse-up - i.e.
    /// while a text-selection drag is genuinely in progress (GitHub issue #158). Gates
    /// [`Self::handle_mouse_move`] so an ordinary hover never extends a selection; the
    /// selection *itself* lives in `alacritty_terminal`'s own `Term::selection` (see
    /// `crate::terminal::grid`'s selection docs), not here.
    selecting: bool,
    /// Test-only seam: the exact [`TerminalPalette`] the most recent real `Render::render` call
    /// painted with (GitHub issue #208). Written by `render` itself and by nothing else, so a test
    /// asserting on it is asserting on what the pane genuinely painted rather than re-deriving the
    /// palette a second way and hoping the two agree.
    #[cfg(test)]
    last_painted_palette: Option<TerminalPalette>,
    /// The scrollback scrollbar's own [`ScrollableHandle`] adapter (GitHub issue #331) - see
    /// [`TerminalScrollHandle`]'s own docs for why the terminal needs one at all, rather than
    /// reusing a plain `gpui::ScrollHandle` the way every other scrollable region does.
    scroll_handle: TerminalScrollHandle,
    /// Accumulated, not-yet-applied mouse-wheel/trackpad scroll distance, in pixels (GitHub
    /// issue #331). A mouse-wheel notch (`gpui::ScrollDelta::Lines`) always converts to a whole
    /// number of grid lines cleanly, but trackpad deltas (`ScrollDelta::Pixels`) essentially
    /// never divide evenly by [`Self::line_height_px`] - truncating every single event to whole
    /// lines would silently drop each event's sub-line remainder, and a slow trackpad flick
    /// would scroll nothing at all. This carries that remainder across events; see
    /// [`Self::handle_scroll_wheel`].
    pending_scroll_px: f32,
    /// `true` once real pty output has arrived while [`Self::grid`] was scrolled back (GitHub
    /// issue #331) - drives the "jump to bottom" affordance's highlighted "there's new output"
    /// state. Cleared the moment [`Self::grid`]'s own scroll offset is observed back at live
    /// (`render`, every frame - self-correcting, since the grid is the single source of truth
    /// for "are we scrolled back", never mirrored here).
    new_output_while_scrolled: bool,
    /// `true` once [`Self::maybe_resize_pty`] has applied a resize computed from a real,
    /// measured [`Self::content_bounds`] rather than the pre-paint `window.viewport_size()`
    /// fallback (GitHub issue #362) - see that method's own docs for why the very first such
    /// resize discards whatever scrollback the placeholder-sized spawn window may have
    /// manufactured. Latched permanently `true` after that one discard so a later, real
    /// user-driven resize (an actual window resize, a font-size change) is never touched by it.
    settled_real_size: bool,
}

impl TerminalPane {
    /// `font_size_px` is the caller-supplied starting font size - every production caller
    /// (`crate::work_surface::agents::Agents::spawn`) passes the live
    /// `crate::settings::store::AppearanceSettings::terminal_font_size`, never a hardcoded
    /// literal.
    /// Clamped via [`sanitized_font_size_px`] the same way [`Self::set_font_size`] clamps a
    /// later edit, so an already out-of-range persisted value (a hand-edited settings file)
    /// can never reach font-metrics measurement.
    pub fn new(spec: TerminalSpec, font_size_px: f32, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            spec,
            grid: TerminalGrid::new(TERMINAL_ROWS, TERMINAL_COLS),
            session: None,
            spawn_error: None,
            exit_status: None,
            activity_at: None,
            attention_ping_at: None,
            eof_pending: false,
            eof_poll_ticks: 0,
            focus_handle: cx.focus_handle(),
            content_bounds: None,
            font_size_px: sanitized_font_size_px(font_size_px),
            cell_width_px: None,
            resize_latch: ResizeLatch::default(),
            _task: None,
            is_foreground: true,
            selecting: false,
            #[cfg(test)]
            last_painted_palette: None,
            scroll_handle: TerminalScrollHandle::new(),
            pending_scroll_px: 0.0,
            new_output_while_scrolled: false,
            settled_real_size: false,
        };
        this.spawn_process(cx);
        this
    }

    /// Applies a Settings › Appearance "Terminal font size" edit
    /// (`crate::root::AdeApp`'s `adjust_terminal_font_size`, via
    /// `crate::work_surface::agents::Agents::set_terminal_font_size`) to this already-live pane - a
    /// no-op if the sanitized value is unchanged (e.g. a stepper click already at a clamp
    /// boundary).
    pub fn set_font_size(&mut self, font_size_px: f32, cx: &mut Context<Self>) {
        let font_size_px = sanitized_font_size_px(font_size_px);
        if font_size_px == self.font_size_px {
            return;
        }
        self.font_size_px = font_size_px;
        self.cell_width_px = None;
        cx.notify();
    }

    /// The line height for [`Self::font_size_px`] - keeps the design's `12px/19` ratio rather
    /// than a second, independently-chosen line-height setting this app doesn't have.
    fn line_height_px(&self) -> f32 {
        self.font_size_px * (ROW_LINE_HEIGHT_PX / ROW_FONT_SIZE_PX)
    }

    /// Deterministically tears down the current child process, if any, via
    /// `PtySession::shutdown` (blocks until the process tree is confirmed dead and reaped) -
    /// run on the background executor so this doesn't block the GPUI foreground thread.
    /// Intended for closing a tab: called before the owning `Entity<TerminalPane>` is dropped,
    /// so process teardown is a completed, verified fact rather than left to `Drop`'s
    /// fire-and-forget signal-then-detach behavior (still correct and non-leaking on its own,
    /// per `pty-core`'s docs, just not deterministic about *when* the process is reaped).
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

    /// `true` while a child process is alive (spawned and not yet observed to have exited).
    /// The rail's status derivation (`crate::rail::status::derive_status`) uses this to distinguish
    /// a still-running session from one that has exited or never started.
    pub fn is_running(&self) -> bool {
        self.session.is_some()
    }

    /// How long it has been since this pane's process last produced output (or since it
    /// started, if it hasn't produced any yet). `None` if no process is running - callers that
    /// need "no process" as a distinct case should check [`Self::is_running`] first; see
    /// `crate::rail::state`'s `process_signal`.
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

    /// The exit status of this pane's process, once observed - see [`Self::exit_status`]'s
    /// field docs for exactly when this becomes `Some`.
    pub fn exit_status(&self) -> Option<&ExitStatus> {
        self.exit_status.as_ref()
    }

    /// The window title this pane's process last set via OSC 0/2, if any (GitHub issue #239).
    pub fn title(&self) -> Option<&str> {
        self.grid.title()
    }

    /// Whether this pane's process has fired an OSC 9 / OSC 777 desktop notification that the
    /// human hasn't answered yet - see [`Self::attention_ping_at`]'s field docs for the exact
    /// lifecycle, and `crate::terminal::osc` for the sequences involved.
    pub fn has_pending_attention_ping(&self) -> bool {
        self.attention_ping_at.is_some()
    }

    /// The most recent OSC 9;4 progress report from this pane's process, if it speaks that
    /// protocol - see [`crate::terminal::osc::Progress`].
    pub fn progress(&self) -> Option<Progress> {
        self.grid.progress()
    }

    /// Marks this pane's outstanding attention ping as answered - called when the human
    /// actually responds to it, which is when they type into this pane, and when a new process
    /// takes the pane over. See [`Self::attention_ping_at`]'s field docs.
    fn clear_attention_ping(&mut self) {
        self.attention_ping_at = None;
    }

    /// Tells this pane whether its agent is the globally active tab - see
    /// [`Self::is_foreground`]'s field docs. Called (only) by
    /// `crate::work_surface::agents::Agents::sync_pane_cadence` on every active-agent change; the poll
    /// loop reads the flag afresh each tick, so a change takes effect within one tick of
    /// whichever cadence the pane was on. Deliberately no `cx.notify()`: the flag changes
    /// polling cadence, never anything rendered.
    pub fn set_foreground(&mut self, foreground: bool) {
        self.is_foreground = foreground;
    }

    /// Whether this pane currently polls at the foreground cadence - see
    /// [`Self::is_foreground`]'s field docs. Test-only observation point (this crate's
    /// cadence tests); no production reader exists - the poll loop reads the field directly -
    /// so this is `#[cfg(test)]` rather than shipping dead code.
    #[cfg(test)]
    pub(crate) fn is_foreground(&self) -> bool {
        self.is_foreground
    }

    /// The error from the most recent failed spawn attempt, if any. A process that never
    /// started has no [`Self::exit_status`] to report, but is still a failure the rail's
    /// status derivation should surface - see `crate::rail::state`'s `process_signal`.
    pub fn spawn_error(&self) -> Option<&str> {
        self.spawn_error.as_deref()
    }

    /// The resolved program name this pane was spawned with (e.g. `claude`, `codex`, or
    /// whatever `$SHELL` resolved to), used by the CLI/terminal tab label and pane header.
    /// Falls back to the full path's display form only if `Path::file_name` returns `None`
    /// (practically unreachable, since [`TerminalSpec::shell`]/`::command` always hand this a
    /// non-empty path).
    pub fn program_label(&self) -> String {
        match self.spec.program.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => self.spec.program.display().to_string(),
        }
    }

    /// The child process's OS pid, once spawned - `PtySession::process_id`, `None` before a
    /// process exists or if the platform doesn't expose one. Backs the CLI pane header's
    /// `pid 48213` display.
    pub fn pid(&self) -> Option<u32> {
        self.session
            .as_ref()
            .and_then(|session| session.process_id())
    }

    /// Sends `Ctrl-C` (`0x03`) to the child process's pty, exactly as [`Self::handle_key_down`]
    /// would for an actual `Ctrl-C` keystroke - backs the rail agent menu's `Pause` row. A
    /// no-op when no session is live.
    pub fn interrupt(&mut self, _cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        // A failed write is logged, not stored in `Self::spawn_error` - that field means "the
        // process never started" and is only read while `!self.is_running()`, which can't be
        // true here. Storing it there would be the wrong field semantically even though
        // nothing reads it that way today.
        if let Err(err) = session.write_input(&[0x03]) {
            log::warn!("failed to send interrupt: {err}");
        }
    }

    /// The current `(cols, rows)` this pane's grid is sized to - backs the terminal info
    /// footer's `148×38`-style dimensions display. Delegates to [`TerminalGrid::dimensions`],
    /// already a tracked fact (see [`Self::resize_to`]), not recomputed here.
    pub fn grid_dimensions(&self) -> (u16, u16) {
        self.grid.dimensions()
    }

    /// Clears the terminal - the header's `clear` hint action (a click-only, not
    /// global-keybinding, affordance). Delegates to [`TerminalGrid::clear`] and notifies so
    /// the now-empty grid repaints.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.grid.clear();
        if let Some(session) = &self.session {
            if let Err(err) = session.write_input(&[0x0c]) {
                log::warn!("failed to send clear-redraw signal (Ctrl-L) to pty: {err}");
            }
        }
        cx.notify();
    }

    /// Suspends this pane's real child process in place via `PtySession::pause` (a real
    /// `SIGSTOP`), without killing it - GitHub issue #242 phase B's interactive-rebase UI's
    /// "Pause now" action, used to freeze a running agent process while a rebase rewrites files
    /// out from under it. A no-op (not an error) when no session is live yet. See
    /// [`Self::resume`] and `pty_core::PtySession::pause`'s own docs for the platform scope
    /// (unix only - a real, honest error on a platform with no `SIGSTOP` equivalent).
    pub fn pause(&self) -> Result<(), PtyError> {
        match &self.session {
            Some(session) => session.pause(),
            None => Ok(()),
        }
    }

    /// Resumes a process this pane's own [`Self::pause`] suspended, via a real `SIGCONT`. Safe
    /// to call even if the process was never actually paused - see `pty_core::PtySession::
    /// resume`'s own docs.
    pub fn resume(&self) -> Result<(), PtyError> {
        match &self.session {
            Some(session) => session.resume(),
            None => Ok(()),
        }
    }

    // ------------------------------------------------------------ clipboard (GitHub issue #158)

    /// Writes this pane's real current selection to the real OS clipboard
    /// (`gpui::App::write_to_clipboard`, the same call `crate::sidebar::tree_ops::AdeApp::
    /// copy_path_to_system_clipboard` already uses for "Copy Path" - one clipboard mechanism in
    /// this app, not two). Returns whether anything was actually copied.
    pub fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.grid.selected_text() else {
            return false;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        true
    }

    /// Reads the real OS clipboard and feeds it to the child process through
    /// `PtySession::write_input` - the exact same path [`Self::handle_key_down`] writes typed
    /// keystrokes through, so a paste is indistinguishable to the child from very fast typing.
    /// Returns whether bytes were actually written.
    pub fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        let Some(session) = &self.session else {
            return false;
        };
        let payload = paste_payload(&text, self.grid.bracketed_paste_enabled());
        if let Err(err) = session.write_input(payload.as_bytes()) {
            self.spawn_error = Some(format!("failed to write input: {err}"));
            cx.notify();
            return false;
        }
        true
    }

    /// Whether the program running in this pane currently has bracketed paste on (`DECSET 2004`).
    pub fn bracketed_paste_enabled(&self) -> bool {
        self.grid.bracketed_paste_enabled()
    }

    /// Delivers `text` into this pane's real pty as **one** prompt, submitted once - the whole of
    /// GitHub issue #288's *"Sending delivers one batched, line-anchored prompt into the target
    /// agent's pty - never one message per note"*.
    pub fn send_prompt(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if text.is_empty() {
            return false;
        }
        let bracketed = self.grid.bracketed_paste_enabled();
        if !bracketed && text.contains(['\n', '\r']) {
            self.spawn_error = Some(
                "refused to send: this program has bracketed paste off, so a multi-line prompt \
                 would arrive as several separate messages"
                    .to_string(),
            );
            cx.notify();
            return false;
        }
        // A prompt is only worth claiming as delivered if there is really something running to
        // read it. `session.is_some()` alone is not that: the poll loop only clears `session`
        // once it has *observed* EOF, which lags the child's real exit by at least a tick plus
        // `MAX_EOF_POLL_TICKS`' grace, and in that window a write into a dead agent would return
        // `true`. GitHub issue #288 flips every note on the file to `sent` on that `true`, with
        // no way back, so this checks the two facts the pane already knows about the child's
        // liveness before it writes anything.
        if self.exit_status.is_some() || self.eof_pending {
            self.spawn_error = Some("refused to send: this agent's process has ended".to_string());
            cx.notify();
            return false;
        }
        let Some(session) = &self.session else {
            return false;
        };
        // Two writes, deliberately, and in this order.
        //
        // The paste and its submit go to the pty as two separate `write_all`+`flush` calls on the
        // writer thread rather than one, so a full-screen agent TUI gets a real read boundary
        // between "here is a pasted blob" and "now send it". These TUIs commit a bracketed paste
        // asynchronously (Ink debounces it), and a CR arriving inside the *same* read chunk can
        // be consumed before the paste has been committed - which drops the submit, or submits a
        // prefix. Ordering is still guaranteed: both go down one `mpsc` to one writer thread.
        if let Err(err) = session.write_input(paste_payload(text, bracketed).as_bytes()) {
            self.spawn_error = Some(format!("failed to write input: {err}"));
            cx.notify();
            return false;
        }
        if let Err(err) = session.write_input(b"\r") {
            self.spawn_error = Some(format!("failed to write input: {err}"));
            cx.notify();
            return false;
        }
        true
    }

    /// Maps a window-space pointer position onto the grid cell under it, or `None` before this
    /// pane has ever painted (no measured [`Self::content_bounds`] to resolve against yet).
    fn cell_position_at(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &Window,
    ) -> Option<CellPosition> {
        let bounds = self.content_bounds?;
        let cell_size = self.cell_size(window);
        let leading_rows = if self.spawn_error.is_some() { 1.0 } else { 0.0 };
        let origin = gpui::point(
            bounds.origin.x + px(PANE_PADDING_PX),
            bounds.origin.y + px(PANE_PADDING_PX) + cell_size.height * leading_rows,
        );
        let (cols, rows) = self.grid.dimensions();
        Some(cell_position_in_grid(
            position, origin, cell_size, rows, cols,
        ))
    }

    /// Anchors a fresh selection under the pointer and takes focus - a real left mouse-down
    /// inside the grid. Anchoring on *every* left mouse-down (not only ones that turn into
    /// drags) is what makes a plain click clear the previous selection, since an undragged
    /// anchor is empty - see [`crate::terminal::grid::TerminalGrid::start_selection`]'s docs.
    fn handle_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some(position) = self.cell_position_at(event.position, window) else {
            return;
        };
        self.grid.start_selection(position);
        self.selecting = true;
        cx.notify();
    }

    /// Extends an in-progress selection to the pointer. Gated on [`Self::selecting`] *and* on
    /// the left button still being held: a mouse-up that happens outside this pane's bounds
    /// never reaches [`Self::handle_mouse_up`] (GPUI's `on_mouse_up` only fires over a hovered
    /// hitbox), so without the second check a drag released elsewhere would leave this pane
    /// extending its selection on every later hover.
    fn handle_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        if event.pressed_button != Some(gpui::MouseButton::Left) {
            self.selecting = false;
            return;
        }
        let Some(position) = self.cell_position_at(event.position, window) else {
            return;
        };
        self.grid.update_selection(position);
        cx.notify();
    }

    fn handle_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.selecting = false;
    }

    /// Mouse-wheel/trackpad scrollback (GitHub issue #331) - converts `event.delta` into whole
    /// grid lines, carrying any sub-line remainder in [`Self::pending_scroll_px`], then either
    /// drives `TerminalGrid::scroll_display` directly or - while a full-screen program has the
    /// alt screen active (GitHub issues #362, #368) - forwards the equivalent number of
    /// PageUp/PageDown presses to the child process instead; see
    /// [`Self::forward_scroll_as_page_keys`]'s own docs for why.
    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row_height = self.line_height_px();
        if row_height <= 0.0 {
            return;
        }
        let delta_px = event.delta.pixel_delta(px(row_height)).y.as_f32();
        self.pending_scroll_px += delta_px;
        let lines = (self.pending_scroll_px / row_height).trunc();
        if lines == 0.0 {
            return;
        }
        self.pending_scroll_px -= lines * row_height;
        if self.grid.alt_scroll_forwarding_active() {
            self.forward_scroll_as_page_keys(lines as i32);
        } else {
            self.grid.scroll_display(ScrollAmount::Lines(lines as i32));
        }
        cx.notify();
    }

    /// Translates a mouse-wheel scroll delta into the same PageUp/PageDown byte sequence
    /// [`keystroke_to_bytes`] already produces for a real page-key press, and writes it straight
    /// to the pty (GitHub issues #362, #368) - reusing that one encoder rather than a second,
    /// hand-rolled `\x1b[5~`/`\x1b[6~` literal here, so the two can never drift apart.
    fn forward_scroll_as_page_keys(&mut self, lines: i32) {
        let Some(session) = &self.session else {
            return;
        };
        let key = if lines > 0 { "pageup" } else { "pagedown" };
        let Some(single_press) = keystroke_to_bytes(&Keystroke {
            key: key.to_string(),
            key_char: None,
            modifiers: Modifiers::default(),
        }) else {
            return;
        };
        let presses =
            ((lines.unsigned_abs() as f32 / WHEEL_LINES_PER_NOTCH).round() as usize).max(1);
        let mut bytes = Vec::with_capacity(single_press.len() * presses);
        for _ in 0..presses {
            bytes.extend_from_slice(&single_press);
        }
        if let Err(err) = session.write_input(&bytes) {
            self.spawn_error = Some(format!("failed to write input: {err}"));
        }
    }

    /// Applies a scrollbar click/drag's requested target, if one was recorded since the last
    /// render (see [`TerminalScrollHandle`]'s own docs for why this is deferred by a frame
    /// rather than applied immediately). Called at the top of every `render`, before this
    /// frame's rows are read out of [`Self::grid`], so a click/drag is reflected in the very
    /// same frame it's applied in rather than lagging an extra one.
    fn apply_pending_scrollbar_target(&mut self) {
        if let Some(target) = self.scroll_handle.take_requested_display_offset() {
            self.grid.set_scroll_offset(target);
        }
    }

    /// The "jump to bottom" affordance (GitHub issue #331) - `None` (nothing painted, nothing
    /// hit-tested) whenever [`TerminalGrid::is_scrolled_back`] is `false`, since there is
    /// nothing to jump to: this only ever appears while genuinely showing scrollback, matching
    /// [`crate::root::scrollbar::render_vertical_scrollbar`]'s own "`None` when there's nothing
    /// to act on" convention.
    fn render_jump_to_bottom_affordance(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.grid.is_scrolled_back() {
            return None;
        }
        let highlighted = self.new_output_while_scrolled;
        let (fg, border, label): (gpui::Rgba, gpui::Rgba, &'static str) = if highlighted {
            (
                theme::status::RUN.into(),
                theme::status::RUN.into(),
                "New output \u{2193}",
            )
        } else {
            (
                theme::text::SECONDARY.into(),
                theme::border::POPOVER.into(),
                "\u{2193} Scrolled",
            )
        };
        Some(
            div()
                .id("terminal-jump-to-bottom")
                .absolute()
                .bottom(px(10.0))
                .right(px(scrollbar::CONTENT_CLEARANCE))
                .flex()
                .items_center()
                .gap(px(5.0))
                .h(px(20.0))
                .px(px(8.0))
                .rounded(theme::radius::CARD_SM)
                .bg(theme::surface::POPOVER)
                .border_1()
                .border_color(border)
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::MENU_ROW_HOVER))
                .font(font(theme::font::SANS))
                .text_size(px(10.5))
                .text_color(fg)
                .child(label)
                .tooltip(text_tooltip("Jump to the live output"))
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.grid.scroll_display(ScrollAmount::Bottom);
                    this.new_output_while_scrolled = false;
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// Test-only seam: feeds bytes straight into this pane's grid, exactly as
    /// [`Self::spawn_process`]'s poll loop does for pty output - lets a test put known,
    /// deterministic text on screen without synchronizing against a real child process's
    /// timing.
    #[cfg(test)]
    pub(crate) fn inject_bytes_for_test(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.grid.append_bytes(bytes);
        cx.notify();
    }

    /// Test-only seam: the pane's real current selection text, so a test outside this module
    /// can assert on what a simulated drag actually selected.
    #[cfg(test)]
    pub(crate) fn selected_text_for_test(&self) -> Option<String> {
        self.grid.selected_text()
    }

    /// Test-only seam: anchors and drags a selection over the given cell span, exactly as
    /// [`Self::handle_mouse_down`]/[`Self::handle_mouse_move`] do - lets a test outside this
    /// module (e.g. the `TerminalCopy` action's own coverage) set up a real selection without
    /// also depending on painted pixel geometry.
    #[cfg(test)]
    pub(crate) fn select_cells_for_test(&mut self, row: usize, columns: std::ops::Range<usize>) {
        self.grid.start_selection(CellPosition {
            row,
            column: columns.start,
            side: CellSide::Left,
        });
        self.grid.update_selection(CellPosition {
            row,
            column: columns.end.saturating_sub(1),
            side: CellSide::Right,
        });
    }

    /// Test-only seam: this pane's measured content-area bounds - lets a test compute a click
    /// position from the pane's actual painted geometry instead of a guessed pixel offset.
    /// `None` before the pane has painted at least once.
    #[cfg(test)]
    pub(crate) fn content_bounds_for_test(&self) -> Option<Bounds<Pixels>> {
        self.content_bounds
    }

    /// Test-only seam: this pane's measured monospace cell size (the same measurement `render`
    /// uses every frame), so a test can compute exactly where a given row/column lands.
    #[cfg(test)]
    pub(crate) fn cell_size_for_test(&mut self, window: &Window) -> Size<Pixels> {
        self.cell_size(window)
    }

    /// Test-only seam: `(Self::resize_latch.grid, Self::resize_latch.session)` - see
    /// [`ResizeLatch`]'s docs. A font-size-change test reads `.1` (the session half) to prove
    /// the child pty was actually informed of a new size, not just that [`Self::grid`]
    /// repainted while the process underneath it kept believing the old one.
    #[cfg(test)]
    pub(crate) fn resize_sync_state_for_test(&self) -> (Option<GridDims>, Option<GridDims>) {
        (self.resize_latch.grid, self.resize_latch.session)
    }

    /// Test-only seam: this pane's current font size in pixels.
    #[cfg(test)]
    pub(crate) fn font_size_px_for_test(&self) -> f32 {
        self.font_size_px
    }

    /// Test-only seam: the real [`TerminalSpec`] this pane was constructed with - lets a test
    /// outside this module (GitHub issue #227's resume flow) assert on the actual program/args/env
    /// a real [`crate::work_surface::agents::Agents::spawn`]/`spawn_resume` call produced, rather
    /// than only on its side effects.
    #[cfg(test)]
    pub(crate) fn spec_for_test(&self) -> &TerminalSpec {
        &self.spec
    }

    /// Test-only seam: the palette the most recent real paint used - see
    /// [`Self::last_painted_palette`].
    #[cfg(test)]
    pub(crate) fn last_painted_palette_for_test(&self) -> Option<TerminalPalette> {
        self.last_painted_palette
    }

    /// Test-only seam: the visible grid resolved against the palette the most recent real paint
    /// used, i.e. exactly the cells that paint turned into styled spans.
    #[cfg(test)]
    pub(crate) fn painted_rows_for_test(&self) -> Vec<Vec<GridCell>> {
        let palette = self
            .last_painted_palette
            .expect("the pane must have painted at least once");
        self.grid.visible_rows(&palette)
    }

    /// The visible grid as trimmed-right text lines (leading whitespace, e.g. an agent CLI's
    /// indented menu, is preserved) - used by `crate::rail::state` to build the "question preview"
    /// (the design's "Jerry reading the tail of the agent's pty").
    pub fn visible_text_lines(&self) -> Vec<String> {
        self.grid
            // Text only - this reads nothing but `GridCell::c`, so which palette the cells were
            // resolved against genuinely cannot affect the result, and this caller (`crate::rail::
            // state`'s question preview) has no live theme access of its own to offer.
            .visible_rows(&TerminalPalette::default())
            .iter()
            .map(|row| {
                row.iter()
                    // A wide character's trailing spacer holds a `' '` the *emulator* wrote, not
                    // one the program printed (GitHub issue #211) - keeping it would put a stray
                    // space after every CJK character/emoji in the rail's question preview.
                    .filter(|cell| cell.width != CellWidth::Spacer)
                    .map(|cell| cell.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Everything this pane's process has really printed that the emulator still holds, capped to
    /// the last `max_lines` - see [`crate::terminal::grid::TerminalGrid::retained_text_lines`].
    pub fn retained_text_lines(&self, max_lines: usize) -> Vec<String> {
        self.grid.retained_text_lines(max_lines)
    }

    fn spawn_process(&mut self, cx: &mut Context<Self>) {
        let spec = self.spec.clone();
        let program_for_error = spec.program.clone();
        let task = cx.spawn(async move |this, cx| {
            let spawn_result: Result<PtySession, PtyError> = cx
                .background_executor()
                .spawn(async move {
                    let mut options = SpawnOptions::new(spec.program)
                        .args(spec.args)
                        .cwd(spec.cwd)
                        .size(TERMINAL_ROWS, TERMINAL_COLS);
                    for (key, value) in spec.env {
                        options = options.env(key, value);
                    }
                    pty_core::spawn(options)
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
                    // A freshly started process hasn't produced output yet, but it just
                    // demonstrably did something (started) - counting that as activity keeps
                    // a still-spawning agent from being immediately misread as long-idle by
                    // `crate::rail::status::derive_status`.
                    this.activity_at = Some(std::time::Instant::now());
                    // A previous process's unanswered attention ping is not this one's - see
                    // `attention_ping_at`'s field docs.
                    this.clear_attention_ping();
                    // The pane may already have rendered (and computed a target grid size)
                    // before this task's background spawn finished - there was no live
                    // session yet for that call to reach, so retry it now that one exists,
                    // rather than waiting for the next resize to reach the pty.
                    if let Some(target) = this.resize_latch.grid {
                        this.resize_to(target.0, target.1);
                    }
                    cx.notify();
                })
                .is_err()
            {
                return; // the pane was dropped before the process finished starting
            }

            // The pane starts foreground (see `Self::is_foreground`'s field docs), so the first
            // tick uses the foreground interval; every later tick re-reads the live flag via
            // the `tick_cadence` the previous tick returned.
            let mut next_interval = POLL_INTERVAL;
            loop {
                cx.background_executor().timer(next_interval).await;

                let poll_result = this.update(cx, |this, cx| {
                    // Budget from the cadence at the tick's *start*; the interval returned at
                    // the bottom is recomputed after any EOF transition this tick made, so a
                    // background pane that just saw EOF starts its exit-confirmation grace
                    // countdown at the foreground interval `MAX_EOF_POLL_TICKS` is derived
                    // from immediately, not one background tick later.
                    let (_, drain_budget) = tick_cadence(this.is_foreground, this.eof_pending);
                    let mut appended = false;
                    let mut process_ended = false;
                    // Captured inside the `this.session.as_mut()` borrow below (so it can
                    // call `&mut self` `PtySession::try_wait`) and only written back to
                    // `this.exit_status` once that borrow has ended.
                    let mut newly_exited: Option<ExitStatus> = None;

                    if this.eof_pending {
                        // EOF was already observed on a previous tick but the exit status
                        // wasn't confirmed yet - see `eof_poll_decision`'s docs for the race
                        // this branch handles. `Self::session` is deliberately still `Some`
                        // here: the process may genuinely still be alive.
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
                        // Capped at this tick's cadence budget (`MAX_BYTES_PER_TICK` or its
                        // background counterpart - see those constants' docs), not drained to
                        // empty. Anything left in the channel is picked up next tick.
                        let mut drained_bytes = 0usize;
                        while drained_bytes < drain_budget {
                            match session.output().try_recv() {
                                Ok(chunk) => {
                                    // `.max(1)` so the loop is bounded by its own iteration
                                    // count too, not only by bytes: a zero-length chunk would
                                    // otherwise never advance `drained_bytes`. `pty-core`
                                    // never sends one (its reader treats `read` returning 0 as
                                    // EOF and breaks), but a drain bound that silently becomes
                                    // unbounded if that ever changes is not a bound.
                                    drained_bytes += chunk.len().max(1);
                                    this.grid.append_bytes(&chunk);
                                    // GitHub issue #331: real output arrived while the human was
                                    // looking at scrollback - latch the "new output" indicator
                                    // for the jump-to-bottom affordance. `Self::grid` already
                                    // stays pinned to the same historical lines on its own (see
                                    // `TerminalGrid::scroll_display`'s docs), so this is purely
                                    // the UI signal, never a scroll-position decision.
                                    if this.grid.is_scrolled_back() {
                                        this.new_output_while_scrolled = true;
                                    }
                                    this.activity_at = Some(Instant::now());
                                    // Consume the grid's one-shot OSC 9 / 777 notification flag
                                    // right where the bytes that could have set it were parsed,
                                    // and latch it - see `attention_ping_at`'s field docs for
                                    // why this must not be consumed from the render path.
                                    if this.grid.take_attention_ping() {
                                        this.attention_ping_at = Some(Instant::now());
                                    }
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

                        // Any bytes the VT parser itself generated while processing the chunks
                        // just appended above - e.g. a cursor position report for `ESC[6n` -
                        // must be written back to the pty's own stdin, not just left in
                        // `this.grid`. This is never a no-op-safe skip: real Windows ConPTY
                        // sends exactly this query as part of its own startup handshake and
                        // blocks its entire output stream on a real answer (confirmed live -
                        // see `TermEventSink`'s own docs), so a dropped reply here doesn't just
                        // misrender a query response, it silently hangs the whole pane forever.
                        if appended {
                            let pending_writes = this.grid.take_pending_pty_writes();
                            if !pending_writes.is_empty() {
                                if let Err(err) = session.write_input(&pending_writes) {
                                    log::warn!(
                                        "failed to answer a terminal query (e.g. cursor \
                                         position report) back to the pty: {err}"
                                    );
                                }
                            }
                        }

                        // Windows has no channel-disconnect signal to react to, ever - see
                        // `pty_core`'s crate-level "Platform scope" docs. On unix, a dead
                        // child's reader thread hits real pty EOF quickly, which closes
                        // `output_tx` and trips the `TryRecvError::Disconnected` arm above.
                        // On Windows, `run_reader_loop`'s reader thread only observes EOF once
                        // `PtySession::master` itself is dropped - which killing/reaping the
                        // child does NOT do on its own (see that function's docs for the full
                        // ConPTY/`ClosePseudoConsole` ownership chain) - so a killed Windows
                        // process would otherwise leave `session` `Some` forever and this poll
                        // loop would spin indefinitely believing it was still running:
                        // `is_running()` stuck `true`, `exit_status()` stuck `None`,
                        // `grid.mark_ended()` never called. So Windows independently polls
                        // `PtySession::try_wait` directly, every tick, regardless of channel
                        // state - a real, cheap, non-blocking `GetExitCodeProcess` check (see
                        // `pty_core`'s docs) that works whether or not the pty's own I/O ever
                        // signals anything. Reuses the exact same `newly_exited`/
                        // `process_ended` transition the unix EOF path above uses, rather than
                        // a second, parallel one.
                        #[cfg(windows)]
                        if !process_ended {
                            if let Ok(Some(status)) = session.try_wait() {
                                newly_exited = Some(status);
                                process_ended = true;
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

                    // Keep polling while there's a live session, or while still waiting on a
                    // final exit status after EOF (`this.session` can be `Some` while
                    // `eof_pending` is `true`, so the two conditions aren't redundant).
                    let keep_polling = this.session.is_some() || this.eof_pending;
                    let (interval, _) = tick_cadence(this.is_foreground, this.eof_pending);
                    (keep_polling, interval)
                });

                match poll_result {
                    Ok((true, interval)) => next_interval = interval,
                    Ok((false, _)) => break, // the child process exited; nothing left to poll
                    Err(_) => break,         // the pane entity was dropped
                }
            }
        });

        self._task = Some(task);
    }

    /// Forwards a typed key to the child process via `PtySession::write_input`. See the
    /// module docs' "Input" section for the (deliberately small) subset of keys handled.
    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // GitHub issue #331: plain (unmodified) PageUp/PageDown scroll real retained
        // scrollback rather than being forwarded to the pty. Modified variants
        // (Shift/Ctrl/Alt+PageUp, ...) fall through unclaimed, same as before that issue.
        //
        // GitHub issue #368 narrows the claim to the *normal* screen. Issue #331 reasoned that
        // claiming these keys was free because "a PageUp/PageDown reaching a full-screen program
        // is already a real no-op" - which was true only because `keystroke_to_bytes` had no
        // mapping for either key at the time, and stopped being true the moment it gained one.
        // The alt screen keeps no scrollback of its own (`TerminalGrid::alt_screen_active`'s
        // docs), so claiming a page key there costs the human a keystroke and gives them
        // literally nothing back: `scroll_display` cannot move, and the running program - which
        // is the only thing that *can* scroll its own view - never sees the key. That is exactly
        // what Claude Code's CLI meant by its own on-screen "use PgUp/PgDn to scroll" hint: it
        // was telling the user to press keys this app was silently eating. On the alt screen
        // these now fall through to the ordinary `keystroke_to_bytes` write below.
        if !event.keystroke.modifiers.modified() && !self.grid.alt_screen_active() {
            let scroll = match event.keystroke.key.as_str() {
                "pageup" => Some(ScrollAmount::PageUp),
                "pagedown" => Some(ScrollAmount::PageDown),
                _ => None,
            };
            if let Some(scroll) = scroll {
                self.grid.scroll_display(scroll);
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }

        let Some(session) = &self.session else {
            return;
        };
        let Some(bytes) = keystroke_to_bytes(&event.keystroke) else {
            return;
        };
        // Bound rather than matched inline so the `&self.session` borrow ends before
        // `clear_attention_ping` takes `&mut self` below.
        let write_result = session.write_input(&bytes);
        // The human is typing at this pane: whatever the agent pinged for attention about, this
        // is them answering it. Placed after `keystroke_to_bytes` so a keystroke that isn't real
        // terminal input (a bare modifier, an unmapped key) doesn't count as an answer. See
        // `attention_ping_at`'s field docs.
        self.clear_attention_ping();
        if let Err(err) = write_result {
            self.spawn_error = Some(format!("failed to write input: {err}"));
            cx.notify();
        }
        // GitHub issue #331: typing while scrolled back jumps back to the live tail - the
        // standard terminal convention (iTerm2, Alacritty, Windows Terminal all do this): a real
        // keystroke reaching the child process is the human demonstrating they want to interact
        // with the live process, not keep reading history underneath it.
        if self.grid.is_scrolled_back() {
            self.grid.scroll_display(ScrollAmount::Bottom);
        }
        // Consumed as terminal input; don't let it also be interpreted as an app-level
        // keybinding (matches `vendor/zed/crates/terminal_view/src/terminal_view.rs`'s
        // `key_down` handler, which calls `stop_propagation()` after a consumed keystroke).
        cx.stop_propagation();
    }

    /// This pane's measured monospace cell size at `ROW_FONT_SIZE_PX`/`ROW_LINE_HEIGHT_PX`.
    /// The width comes from GPUI's font-metrics API, `Window::text_system().advance`
    /// (verified against `vendor/zed/crates/terminal_view/src/terminal_element.rs:1284`, which
    /// computes its own terminal's `cell_width` the same way) - not a guess. The height is
    /// [`ROW_LINE_HEIGHT_PX`] directly: [`render_row`] sets that as this pane's explicit
    /// `.line_height()`, so there's nothing to measure for height.
    fn cell_size(&mut self, window: &Window) -> Size<Pixels> {
        let width = match self.cell_width_px {
            Some(width) => width,
            None => {
                let font_id = window
                    .text_system()
                    .resolve_font(&font(crate::theme::font::MONO));
                let measured = window
                    .text_system()
                    .advance(font_id, px(self.font_size_px), 'm')
                    .map(|advance| advance.width)
                    .ok()
                    .filter(|width| *width > px(0.0));
                // The fallback scales with the font size too - `APPROX_CELL_WIDTH_PX` is only
                // a guess for the *default* 12px size; using it verbatim at another font size
                // would under/over-estimate the cell width proportionally.
                let width = measured.unwrap_or_else(|| {
                    px(APPROX_CELL_WIDTH_PX * self.font_size_px / ROW_FONT_SIZE_PX)
                });
                // `debug!`, not `info!` - useful when diagnosing a sizing bug, silent by
                // default (`main.rs`'s `env_logger` filter defaults to `info`).
                log::debug!(
                    "terminal_pane: measured real cell width = {width:?} at font size \
                     {}px (font-metrics lookup succeeded: {})",
                    self.font_size_px,
                    measured.is_some()
                );
                self.cell_width_px = Some(width);
                width
            }
        };
        gpui::size(width, px(self.line_height_px()))
    }

    /// Recomputes a `(rows, cols)` from this pane's content-area bounds and measured cell size,
    /// then applies it via [`Self::resize_to`]. Called from `render` so it naturally re-runs
    /// whenever the pane's own size changes - not just the window's, since Phase A's
    /// three-zone shell means those are no longer the same thing (see [`size_to_grid`]'s docs
    /// for the bug this distinction fixes).
    fn maybe_resize_pty(&mut self, window: &Window) {
        let raw_size = self
            .content_bounds
            .map(|bounds| bounds.size)
            .unwrap_or_else(|| window.viewport_size());
        let size = content_size_from_padding_box(raw_size);
        let cell_size = self.cell_size(window);
        let (rows, cols) = size_to_grid(size, cell_size);
        let measured_real_size = self.content_bounds.is_some();
        self.resize_to(rows, cols);
        let reached_the_pty = self.resize_latch.session == Some((rows, cols));
        if !self.settled_real_size && measured_real_size && reached_the_pty {
            self.settled_real_size = true;
            self.grid.discard_scrollback();
        }
    }

    /// Applies a target `(rows, cols)` to the grid and, if a live session exists, the child
    /// pty - delegating the "what actually needs to happen" decision to [`ResizeLatch::apply`]
    /// (see its docs for the bug this split prevents), and only calling
    /// [`ResizeLatch::session_resize_succeeded`] once `PtySession::resize` has returned `Ok`,
    /// so a failed resize is retried next time instead of being treated as done.
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

/// Converts a pixel-space size into a `(rows, cols)` terminal grid size, given the
/// caller-measured `cell_size` a single monospace cell renders at (see
/// [`TerminalPane::cell_size`]). A pure function of two [`Size<Pixels>`] values, independent
/// of `Window` or this pane's own state, so it's directly unit-testable (see the tests below).
fn size_to_grid(size: Size<Pixels>, cell_size: Size<Pixels>) -> (u16, u16) {
    let cols = ((size.width.as_f32() / cell_size.width.as_f32()) as u16).max(20);
    let rows = ((size.height.as_f32() / cell_size.height.as_f32()) as u16).max(10);
    (rows, cols)
}

/// Maps a window-space pointer position onto the grid cell under it, given the grid's own
/// painted `origin` (top-left of row 0, column 0), the measured `cell_size`, and the grid's
/// current `rows`/`cols`. Pure - independent of `Window` and this pane's state - so the
/// pixel-to-cell arithmetic is directly unit-testable, the same split [`size_to_grid`] uses.
fn cell_position_in_grid(
    position: gpui::Point<Pixels>,
    origin: gpui::Point<Pixels>,
    cell_size: Size<Pixels>,
    rows: u16,
    cols: u16,
) -> CellPosition {
    // `.max(1.0)`: a degenerate zero cell size can only come from a failed font measurement,
    // and dividing by it would produce `inf`/`NaN` column indexes.
    let cell_width = cell_size.width.as_f32().max(1.0);
    let cell_height = cell_size.height.as_f32().max(1.0);
    let dx = (position.x - origin.x).as_f32().max(0.0);
    let dy = (position.y - origin.y).as_f32().max(0.0);

    let last_row = rows.saturating_sub(1) as usize;
    let last_column = cols.saturating_sub(1) as usize;
    let row = ((dy / cell_height) as usize).min(last_row);
    let raw_column = dx / cell_width;
    let column = (raw_column as usize).min(last_column);
    let side = if raw_column as usize > last_column || raw_column - column as f32 >= 0.5 {
        CellSide::Right
    } else {
        CellSide::Left
    };

    CellPosition { row, column, side }
}

/// Frames clipboard text for writing into a pty, matching
/// `vendor/zed/crates/terminal/src/terminal.rs`'s own `paste` (`:2306`) rather than writing the
/// raw string:
fn paste_payload(text: &str, bracketed_paste: bool) -> String {
    if bracketed_paste {
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

/// Converts [`TerminalPane::content_bounds`]'s raw padding-box measurement into the content-box
/// size glyphs actually render into, by subtracting [`PANE_PADDING_PX`] from each side - see
/// [`TerminalPane::maybe_resize_pty`]'s docs for the padding-box-vs-content-box bug this fixes.
fn content_size_from_padding_box(padding_box: Size<Pixels>) -> Size<Pixels> {
    let padding = px(PANE_PADDING_PX * 2.0);
    gpui::size(padding_box.width - padding, padding_box.height - padding)
}

/// Clamps a terminal font size into `crate::settings::store`'s shared `FONT_SIZE_MIN`/`FONT_SIZE_MAX`
/// bounds - the same range `AppearanceSettings::sanitize` clamps a loaded `settings.toml`
/// value to. A pane has no settings file to defend against a hand-edit, but a font size of
/// `0.0` or less would divide-by-zero-shape a `size_to_grid` call, so this is a defensive
/// second application of the same bound.
fn sanitized_font_size_px(font_size_px: f32) -> f32 {
    font_size_px.clamp(
        crate::settings::store::FONT_SIZE_MIN,
        crate::settings::store::FONT_SIZE_MAX,
    )
}

/// What [`ResizeLatch::apply`] says the caller should actually do for a target size.
#[derive(Debug, PartialEq, Eq)]
struct ResizeActions {
    resize_grid: bool,
    resize_session: bool,
}

/// Tracks, separately, which `(rows, cols)` [`TerminalGrid`] currently reflects and which
/// `(rows, cols)` was last successfully sent to a *live* `PtySession::resize`.
type GridDims = (u16, u16);

#[derive(Debug, Default)]
struct ResizeLatch {
    /// The `(rows, cols)` `TerminalGrid` currently reflects.
    grid: Option<GridDims>,
    /// The `(rows, cols)` last successfully sent to a live session's pty resize - `None`
    /// until a session exists and a resize has actually reached it.
    session: Option<GridDims>,
}

impl ResizeLatch {
    /// Decides what `(rows, cols)` needs applying to the grid and/or a live session's pty, and
    /// latches [`Self::grid`] immediately (grid resizes can't fail). Does *not* latch
    /// [`Self::session`] - only [`Self::session_resize_succeeded`] does that, once the caller
    /// has confirmed a `PtySession::resize` call succeeded.
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

    /// Records that `target` has reached a live session's pty via a successful
    /// `PtySession::resize` call - only after this does [`Self::apply`] treat `target` as
    /// already in sync for the session side.
    fn session_resize_succeeded(&mut self, target: (u16, u16)) {
        self.session = Some(target);
    }
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<TerminalPaneEvent> for TerminalPane {}

/// Converts a typed key into the bytes a real terminal would send for it. A deliberately small
/// subset of `vendor/zed/crates/terminal/src/mappings/keys.rs`'s `to_esc_str` - see the module
/// docs' "Input" section for why the full mapping isn't replicated here. Returns `None` for
/// keys with no reasonable terminal-input meaning (a bare modifier, or a function key this
/// subset doesn't handle).
fn keystroke_to_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    // Never forward a platform (⌘ on macOS, Super/Meta on Linux)-modified keystroke as
    // literal pty input: it would type garbage into the child process, and - since
    // `handle_key_down` calls `cx.stop_propagation()` after a successful write - it would
    // swallow an app-level shortcut (e.g. the rail's secondary-N "new agent") before it
    // reaches its `KeyBinding`, just because a terminal tab happened to have focus. Must run
    // before the fallthrough `key_char` branch below, which returns a character for any
    // keystroke that has one, including platform-modified ones.
    if keystroke.modifiers.platform {
        return None;
    }

    // Ctrl+<letter> control codes (Ctrl-A through Ctrl-Z), e.g. Ctrl-C -> 0x03 (SIGINT),
    // Ctrl-D -> 0x04 (EOF) - the standard terminal mapping
    // (`letter.to_ascii_uppercase() as u8 & 0x1f`). `modifiers.platform` is already excluded
    // by the early return above.
    if keystroke.modifiers.control && !keystroke.modifiers.alt {
        if let Some(ch) = keystroke.key.chars().next() {
            if keystroke.key.chars().count() == 1 && ch.is_ascii_alphabetic() {
                let code = (ch.to_ascii_uppercase() as u8) & 0x1f;
                return Some(vec![code]);
            }
        }
    }

    // Shift+Tab -> the standard CSI back-tab sequence (GitHub issue #236). GPUI reports the
    // same base key (`"tab"`) regardless of Shift, so without this the match below would send
    // plain `\t` for both - indistinguishable to whatever's on the other end of the pty. A real
    // terminal sends `\x1b[Z` for back-tab instead, which is what readline-based tools and
    // Claude Code's own CLI listen for to cycle their mode in the opposite direction. Must run
    // before the `match` below, whose `"tab"` arm has no way to see the modifier.
    //
    // Excludes Ctrl+Shift+Tab (`!modifiers.control`) so that combination's byte stays exactly
    // what it was before this fix - plain `\t`, via the fallthrough `match` below - rather than
    // silently picking up back-tab semantics nobody asked for on top of an unrelated modifier.
    if keystroke.key == "tab" && keystroke.modifiers.shift && !keystroke.modifiers.control {
        return Some(b"\x1b[Z".to_vec());
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
        // The standard xterm CSI `~` sequences for Prior/Next (GitHub issue #368). Added so a
        // full-screen program can actually be *sent* a page key: `TerminalPane::handle_key_down`
        // claims a real PageUp/PageDown for this app's own scrollback only while the normal
        // screen is up, and `TerminalPane::forward_scroll_as_page_keys` synthesises the same two
        // keys from the mouse wheel over the alt screen - both go through this one encoder, so
        // there is a single definition of what a page key is on the wire.
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
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

/// One [`theme::ColorToken`], resolved against whichever theme is really live right now and
/// reduced to the 8-bit-per-channel RGB triple [`TerminalPalette`] carries.
fn token_rgb(token: theme::ColorToken) -> (u8, u8, u8) {
    let color = token.resolve();
    let channel = |component: f32| (component.clamp(0.0, 1.0) * 255.0).round() as u8;
    (channel(color.r), channel(color.g), channel(color.b))
}

/// The live theme's real terminal palette (GitHub issue #208) - `crate::theme::terminal`'s twenty
/// registered tokens resolved against whichever theme is installed right now.
fn theme_terminal_palette() -> TerminalPalette {
    TerminalPalette {
        background: token_rgb(theme::terminal::BACKGROUND),
        foreground: token_rgb(theme::terminal::FOREGROUND),
        cursor: token_rgb(theme::terminal::CURSOR),
        selection: token_rgb(theme::terminal::SELECTION),
        ansi: std::array::from_fn(|index| token_rgb(theme::terminal::ANSI[index])),
    }
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
        // GitHub issue #158: selection is a rendered attribute (it repaints the cell's
        // background), so a run must break at the selection's own edges - merging across one
        // would paint unselected characters as selected.
        && a.selected == b.selected
        // GitHub issue #211: a double-width run is painted at an explicit pixel width covering
        // the two columns per character it owns, so it can never share a span with narrow text -
        // that span would then be sized for the wrong number of columns.
        && a.width == b.width
}

/// One segment of a grid row - either a span of the row's original style-per-cell text, or a
/// detected link. Deliberately holds only char offsets (not `GridCell`s themselves):
/// [`split_segments`] is the pure half of link splitting, kept GPUI/`GridCell`-free so it's
/// directly unit-testable; [`render_row`] turns a segment back into styled cells/elements.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowSegment {
    Plain {
        start: usize,
        end: usize,
    },
    Link {
        start: usize,
        end: usize,
        link: LinkMatch,
    },
}

/// Splits a row of `row_char_count` characters into an ordered, non-overlapping list of plain
/// and link segments, given that row's already-detected links (`crate::terminal::links::
/// find_links`, run against the row's plain text) - the pure half of [`render_row`]'s "a link
/// is a span inside the line, not a whole-line style" contract, so that contract is
/// unit-testable without a live GPUI window. `links` is assumed sorted and non-overlapping,
/// exactly what `find_links` already guarantees.
fn split_segments(row_char_count: usize, links: &[LinkMatch]) -> Vec<RowSegment> {
    let mut segments = Vec::new();
    let mut pos = 0;
    while pos < row_char_count {
        if let Some(link) = links.iter().find(|link| link.start == pos) {
            let end = link.end.min(row_char_count);
            segments.push(RowSegment::Link {
                start: pos,
                end,
                link: link.clone(),
            });
            pos = end;
            continue;
        }
        let mut end = pos + 1;
        // Stop the plain run right before the next link begins, even if the underlying style
        // hasn't changed - `render_row` re-applies `same_run_style` merging *within* this
        // plain segment separately, but must never merge across a link boundary.
        while end < row_char_count && !links.iter().any(|link| link.start == end) {
            end += 1;
        }
        segments.push(RowSegment::Plain { start: pos, end });
        pos = end;
    }
    segments
}

/// `fixed_width` is `Some` only for a double-width run - see [`wide_run_width`].
fn plain_span(
    style: &GridCell,
    text: String,
    palette: &TerminalPalette,
    fixed_width: Option<Pixels>,
) -> impl IntoElement {
    let mut span = div().text_color(rgb(pack_rgb(style.fg)));
    if let Some(width) = fixed_width {
        span = span.w(width).flex_none().whitespace_nowrap();
    }
    // The selection fill wins over the cell's own ANSI background, so a selection is visible
    // across text a program has coloured - which is most of what an agent CLI prints.
    if style.selected {
        span = span.bg(rgb(pack_rgb(palette.selection)));
    } else if style.bg != palette.background {
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
    span.child(text)
}

/// The two paint decisions a caller makes per link span, grouped into one parameter so
/// [`render_link_span`] stays inside clippy's argument-count budget (its id, target and click
/// wiring already account for the rest).
#[derive(Debug, Clone, Copy, Default)]
struct LinkSpanPaint {
    /// The live theme's selection fill when this span falls inside the selection, and `None` when
    /// it doesn't - the fill rather than a `selected: bool` plus the whole palette, since the fill
    /// is the only thing this function ever wants out of it.
    selection_fill: Option<(u8, u8, u8)>,
    /// `Some` only for a double-width run - see [`wide_run_width`].
    fixed_width: Option<Pixels>,
}

/// Renders one detected link as a clickable span - the design's link template:
/// `color:#7fb4e3;border-bottom:1px dotted #3d6a91`, hover `color:#a5cdf0;border-bottom:1px
/// solid #78a8d0`. The link's fixed colour always replaces whatever ANSI colour the underlying
/// cells had, matching the mockup.
fn render_link_span(
    text: String,
    link: &LinkMatch,
    cwd: &Path,
    row_index: usize,
    link_ordinal: usize,
    paint: LinkSpanPaint,
    cx: &mut Context<TerminalPane>,
) -> impl IntoElement {
    let target = terminal_links::resolve(cwd, &link.path);
    let line = link.line;

    let mut span = div()
        .id(format!("terminal-link-{row_index}-{link_ordinal}"))
        .cursor_pointer()
        .text_color(theme::term::LINK)
        .border_b_1()
        .border_color(theme::term::LINK_UNDERLINE)
        .child(text);
    if let Some(fill) = paint.selection_fill {
        span = span.bg(rgb(pack_rgb(fill)));
    }
    if let Some(width) = paint.fixed_width {
        span = span.w(width).flex_none().whitespace_nowrap();
    }
    span.style().border_style = Some(BorderStyle::Dashed);

    span.hover(|mut el| {
        el.style().border_style = Some(BorderStyle::Solid);
        el.text_color(theme::term::LINK_HOVER)
            .border_color(theme::term::LINK_UNDERLINE_HOVER)
    })
    .on_click(cx.listener(move |_this, event: &ClickEvent, _window, cx| {
        if click_included_secondary_modifier(event) {
            cx.emit(TerminalPaneEvent::OpenPath {
                path: target.clone(),
                line,
            });
        }
    }))
}

/// Whether a click held the platform `secondary` modifier at *either* mouse-down or mouse-up,
/// not just mouse-up.
fn click_included_secondary_modifier(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(mouse) => {
            mouse.down.modifiers.secondary() || mouse.up.modifiers.secondary()
        }
        ClickEvent::Keyboard(_) | ClickEvent::Touch(_) => event.modifiers().secondary(),
    }
}

/// One painted piece of a grid row: consecutive cells that share a style, already merged, with
/// the wide characters' spacer cells dropped. The pure half of [`render_row`] - see [`row_runs`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowRun {
    /// Exactly the characters this run paints. Never contains a wide character's spacer `' '`
    /// (see [`CellWidth::Spacer`]), so this is the row's real text, not the emulator's padding.
    text: String,
    /// How many *grid columns* this run covers: one per narrow character, two per wide one. Not
    /// `text.chars().count()` - that is the whole point of this field.
    columns: usize,
    /// The style every cell in the run shares. Its own `GridCell::c` is meaningless here (the
    /// run's characters are in [`Self::text`]); its `width` says whether this is a wide run.
    style: GridCell,
    /// The detected link this run is part of, if any - one run per style change *within* a link,
    /// each keeping the same click target.
    link: Option<LinkMatch>,
}

/// Splits one grid row into the runs [`render_row`] paints - pure, so the whole wide-character
/// layout contract is unit-testable with no GPUI window (see this module's
/// `wide_char_render_tests`).
fn row_runs(row: &[GridCell]) -> Vec<RowRun> {
    let cells: Vec<&GridCell> = row
        .iter()
        .filter(|cell| cell.width != CellWidth::Spacer)
        .collect();
    let text: String = cells.iter().map(|cell| cell.c).collect();
    let links = terminal_links::find_links(&text);

    let mut runs = Vec::new();
    for segment in split_segments(cells.len(), &links) {
        let (start, end, link) = match segment {
            RowSegment::Plain { start, end } => (start, end, None),
            RowSegment::Link { start, end, link } => (start, end, Some(link)),
        };

        let mut inner = start;
        while inner < end {
            let style = *cells[inner];
            let mut run_end = inner + 1;
            // A link's own colour replaces whatever ANSI colour the underlying cells carried, so
            // a link run only has to break where something it *does* paint differs: the selection
            // fill, and the column width. A plain run breaks on any style difference at all.
            let continues_run = |cell: &GridCell| match link {
                Some(_) => cell.selected == style.selected && cell.width == style.width,
                None => same_run_style(cell, &style),
            };
            // A double-width character is deliberately never merged with the next one: each gets
            // its own two-column box, so a glyph whose real advance overshoots two cells (Segoe UI
            // Emoji's do, measurably) can only spill a pixel or two into its immediate neighbour
            // instead of accumulating that overshoot across a whole run and shoving the rest of
            // the row sideways.
            if style.width != CellWidth::Wide {
                while run_end < end && continues_run(cells[run_end]) {
                    run_end += 1;
                }
            }

            runs.push(RowRun {
                text: cells[inner..run_end].iter().map(|cell| cell.c).collect(),
                columns: cells[inner..run_end]
                    .iter()
                    .map(|cell| cell.width.columns())
                    .sum(),
                style,
                link: link.clone(),
            });
            inner = run_end;
        }
    }

    runs
}

/// The explicit pixel width a run must be pinned to, or `None` for an ordinary narrow run that
/// can just take its glyphs' natural advance (GitHub issue #211).
fn wide_run_width(run: &RowRun, cell_width: Pixels) -> Option<Pixels> {
    (run.style.width == CellWidth::Wide).then(|| cell_width * run.columns as f32)
}

/// Renders one grid row as a horizontal run of styled spans - grouping consecutive cells that
/// share the same style into a single span keeps the element count low (a typical row is
/// mostly-uniform default-styled text, so this is usually 1-3 spans, not one per character)
/// even though the underlying grid can be up to `TERMINAL_ROWS` x `TERMINAL_COLS` cells.
/// Additionally splits any run that contains a detected link
/// (`crate::terminal::links::find_links`, via the pure [`split_segments`]) into its own
/// clickable span - see [`render_link_span`]'s docs.
fn render_row(
    row: &[GridCell],
    row_index: usize,
    cwd: &Path,
    palette: &TerminalPalette,
    cell_width: Pixels,
    cx: &mut Context<TerminalPane>,
) -> impl IntoElement {
    let mut line = div().flex().flex_row();
    let mut link_ordinal = 0usize;

    for run in row_runs(row) {
        let fixed_width = wide_run_width(&run, cell_width);
        match &run.link {
            // A selection can end part-way through a link, so the link is split into one span per
            // selected/unselected run rather than highlighted as a unit - every sub-span keeps the
            // same click target, so clicking any part of the link still opens the same file. In
            // the overwhelmingly common (unselected, all-narrow) case this is one span, exactly as
            // it always was.
            Some(link) => {
                line = line.child(render_link_span(
                    run.text,
                    link,
                    cwd,
                    row_index,
                    link_ordinal,
                    LinkSpanPaint {
                        selection_fill: run.style.selected.then_some(palette.selection),
                        fixed_width,
                    },
                    cx,
                ));
                link_ordinal += 1;
            }
            None => {
                line = line.child(plain_span(&run.style, run.text, palette, fixed_width));
            }
        }
    }

    line
}

/// Renders one line of plain (uniformly-coloured) text with the same link detection/
/// click-to-open behavior [`render_row`] gives grid rows - used for the spawn-error message
/// (`Render::render`'s `spawn_error` child), which is plain text `pty_core::spawn` returned,
/// not a `GridCell` grid, but can still contain a path worth linking (e.g. a bad relative path
/// the user typed).
fn render_plain_line_with_links(
    text: &str,
    color: gpui::Rgba,
    cwd: &Path,
    cx: &mut Context<TerminalPane>,
) -> impl IntoElement {
    let links = terminal_links::find_links(text);
    let chars: Vec<char> = text.chars().collect();
    let segments = split_segments(chars.len(), &links);

    let mut line = div().flex().flex_row();
    let mut link_ordinal = 0usize;

    for segment in segments {
        match segment {
            RowSegment::Plain { start, end } => {
                let plain_text: String = chars[start..end].iter().collect();
                line = line.child(div().text_color(color).child(plain_text));
            }
            RowSegment::Link { start, end, link } => {
                let link_text: String = chars[start..end].iter().collect();
                line = line.child(render_link_span(
                    link_text,
                    &link,
                    cwd,
                    // Row index `usize::MAX` keeps this line's link ids in their own,
                    // never-colliding namespace from real grid-row link ids (`render_row`'s
                    // own `row_index` never reaches anywhere close to `usize::MAX`).
                    usize::MAX,
                    link_ordinal,
                    // Neither applies here: the spawn-error line is not part of the grid, so it
                    // is never part of a grid selection, and it is plain `&str` rather than grid
                    // cells, so it has no double-width *columns* to pin a width to - it just
                    // takes its glyphs' natural advance.
                    LinkSpanPaint::default(),
                    cx,
                ));
                link_ordinal += 1;
            }
        }
    }

    line
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.maybe_resize_pty(window);

        // GitHub issue #331: apply a scrollbar click/drag from the previous frame before this
        // frame reads `self.grid`'s scroll state - see `Self::apply_pending_scrollbar_target`'s
        // own docs for why this can't happen synchronously inside the scrollbar's own handlers.
        self.apply_pending_scrollbar_target();
        // The grid's own `display_offset` is the single source of truth for "are we scrolled
        // back" - self-correcting the moment it's observed back at live, rather than needing an
        // explicit "the user jumped back" event from every place that can cause that (the
        // affordance's own click, a keystroke, `Scroll::Bottom` from anywhere else).
        if !self.grid.is_scrolled_back() {
            self.new_output_while_scrolled = false;
        }

        // GitHub issue #211: the real measured monospace advance (`Self::cell_size`, cached), so
        // `render_row` can pin a double-width run to exactly the two columns per character it
        // owns instead of trusting whatever advance the font happens to give that glyph.
        let cell_width = self.cell_size(window).width;

        // GitHub issue #208: resolved once per frame and threaded through every span this paints,
        // so the whole pane (its own fill, unstyled text, every ANSI colour a program asked for,
        // and the selection fill) tracks whichever theme is live - see `theme_terminal_palette`.
        let palette = theme_terminal_palette();
        #[cfg(test)]
        {
            self.last_painted_palette = Some(palette);
        }

        // Measures this pane's rendered content-area bounds every frame, so
        // `Self::maybe_resize_pty` can size the terminal grid from the pane's actual
        // width/height instead of the whole window's - see that method's docs and
        // `size_to_grid`'s docs for the bug this fixes. `.absolute().size_full()` inside a
        // `.relative()` parent, updating `self.content_bounds` from the `canvas()` prepaint
        // callback via a strong `Entity<Self>` handle, is the same idiom
        // `vendor/zed/crates/workspace/src/workspace.rs` uses for its own dock-sizing bounds.
        //
        // **Live report, GitHub issue #375: "the height of the agent terminal is not good"
        // after a genuinely long real conversation.** This `canvas()` prepaint callback runs on
        // every real paint pass regardless of whether `Render::render` was actually re-invoked
        // for this entity - a window resize, a panel opening, a sidebar toggle all re-flow and
        // re-paint the *existing* element tree even when nothing about this pane's own state
        // changed, so `self.content_bounds` genuinely does track the real, current window size
        // the whole time. But nothing used to tell the *next frame* to actually look at that new
        // measurement: `Entity::update` does not implicitly notify (`gpui::Entity::write` calls
        // `cx.notify()` itself precisely because plain `update` doesn't), and even a bare
        // `cx.notify()` called from inside this prepaint callback is silently dropped -
        // `WindowInvalidator::invalidate_view` only actually marks the window dirty when
        // `draw_phase == DrawPhase::None` (`vendor/zed/crates/gpui/src/window.rs`), which is
        // never true while this very prepaint callback is running. Measured directly against
        // this real app: after a real `simulate_resize` on a long-running pane, `content_bounds`
        // updated correctly on the very next paint, yet `TerminalPane::grid_dimensions()` stayed
        // frozen at the *pre-resize* values across several more full `run_until_parked` passes,
        // and only caught up the moment unrelated new pty output (which *does* call
        // `cx.notify()` outside any draw phase, from `Self::spawn_process`'s poll loop) forced a
        // fresh `render()`. A real agent conversation spends real time idle between turns -
        // exactly when a resize/split/sidebar-toggle is likely to happen - so the grid's own
        // row/col count (and therefore the real pty size, and therefore how
        // `alacritty_terminal` itself wraps/paginates the transcript) can silently disagree with
        // the pane's own real, visibly-correct box for as long as that idle period lasts: the
        // chrome and even this pane's own measured bounds look right the whole time, but the
        // grid inside them is still sized for the box that used to be there.
        //
        // `Window::defer` runs its callback "at the end of the current effect cycle" - i.e.
        // after this frame's draw phase has ended - which is exactly the gap `invalidate_view`
        // needs closed: the deferred callback's own `cx.notify()` runs with `draw_phase ==
        // DrawPhase::None`, so it genuinely schedules the next real frame, which then picks the
        // new bounds up in `Self::maybe_resize_pty` like any other real resize does. Comparing
        // against the previous value first keeps this from scheduling a new frame forever while
        // nothing is actually moving.
        let measure_bounds = {
            let this = cx.entity();
            canvas(
                move |bounds, window, cx| {
                    window.defer(cx, move |_window, cx| {
                        this.update(cx, |this, cx| {
                            if this.content_bounds != Some(bounds) {
                                this.content_bounds = Some(bounds);
                                cx.notify();
                            }
                        });
                    });
                },
                |_bounds, _prepaint, _window, _cx| {},
            )
            .absolute()
            .size_full()
        };

        let mut pane = div()
            .id("terminal-pane")
            .debug_selector(|| "terminal-pane".to_string())
            .track_focus(&self.focus_handle)
            // Tagged `"terminal"` (Revision R10) so `crate::default_key_bindings`'s global
            // bindings that have no business firing over a focused terminal (e.g.
            // `CloseFocusedTab`) can scope themselves away from it via `Some("!terminal")` - see
            // that function's own docs for the real conflict this closes: `"ctrl-w"` is
            // `crate::terminal::pane::keystroke_to_bytes`'s own real `unix-word-rerase` control
            // byte, a standard readline word-backspace a focused shell needs unclaimed - a
            // global, unscoped binding would swallow it before it ever reached this pane's own
            // `on_key_down`, the same "app-level shortcut steals terminal input" bug class
            // `crate::default_key_bindings`'s own docs discuss for `secondary-p`/Ctrl+P - unlike
            // that case, which the project ultimately accepted as a deliberate, discussed
            // tradeoff (`TogglePalette` now deliberately claims Ctrl+P unscoped, shadowing
            // readline's own `previous-history`), these bindings instead avoid the collision
            // entirely via this real `!terminal` scoping.
            .key_context("terminal")
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
            }))
            // Real mouse text selection (GitHub issue #158). Left-button only: a right-click
            // has no selection meaning here, and a middle-click is the X11 primary-selection
            // paste gesture this pane deliberately does not claim.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(Self::handle_mouse_down),
            )
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::handle_mouse_up))
            // GitHub issue #331: real mouse-wheel/trackpad scrollback - see
            // `Self::handle_scroll_wheel`'s own docs for why this is a direct handler rather
            // than GPUI's built-in scrollable-div mechanism.
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .bg(rgb(pack_rgb(palette.background)))
            // Not the equivalent `.p_2()` shorthand - see `PANE_PADDING_PX`'s docs for why
            // this needs to be the same value `Self::maybe_resize_pty` subtracts.
            .p(px(PANE_PADDING_PX))
            .font(font(crate::theme::font::MONO))
            // Explicit, not `.text_xs()` - see `ROW_FONT_SIZE_PX`/`ROW_LINE_HEIGHT_PX`'s docs
            // for why leaving either implicit was a measured bug.
            .text_size(px(self.font_size_px))
            .line_height(px(self.line_height_px()))
            .text_color(rgb(pack_rgb(palette.foreground)))
            .child(measure_bounds);

        let cwd = self.spec.cwd.clone();

        if let Some(error) = &self.spawn_error {
            let message = format!("failed to start process: {error}");
            pane = pane.child(render_plain_line_with_links(
                &message,
                theme::terminal::SPAWN_ERROR.resolve(),
                &cwd,
                cx,
            ));
        }

        for (row_index, row) in self.grid.visible_rows(&palette).into_iter().enumerate() {
            pane = pane.child(render_row(&row, row_index, &cwd, &palette, cell_width, cx));
        }

        if self.grid.ended {
            pane = pane.child(
                div()
                    .text_color(theme::terminal::PROCESS_EXITED)
                    .child("[process exited]"),
            );
        }

        // GitHub issue #331: the shared overlay scrollbar (`crate::root::scrollbar`, the same
        // component the file tree/diff view/completions popup/etc. all reuse - see that
        // module's own docs) - synced from the grid's real current scroll state right before
        // it's built, so its geometry reflects this exact frame's numbers, including any
        // `Self::apply_pending_scrollbar_target` call already applied above.
        self.scroll_handle.sync(
            self.content_bounds.unwrap_or_default(),
            px(self.line_height_px()),
            self.grid.scroll_history_len(),
            self.grid.scroll_offset(),
        );
        pane = pane.children(scrollbar::render_vertical_scrollbar(
            "terminal-scrollbar",
            &self.scroll_handle,
            &[],
            cx,
        ));
        pane = pane.children(self.render_jump_to_bottom_affordance(cx));

        pane
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn a_background_pane_drains_on_the_background_interval_read_fresh_each_tick(
        cx: &mut TestAppContext,
    ) {
        let pane = cx.new(|cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", Vec::new(), std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();

        // Fire the loop's first (foreground-armed) tick, demote the pane, then fire one more
        // tick so the demotion is picked up and a BACKGROUND_POLL_INTERVAL sleep is armed.
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.run_until_parked();
        pane.update(cx, |pane, _| pane.set_foreground(false));
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.run_until_parked();

        // Ctrl-L; the pty's ECHOCTL echo ("^L", see
        // `clear_with_a_live_session_sends_a_real_ctrl_l_the_pty_echoes_back`) lands in the
        // output channel within real milliseconds - sleep long enough that it is certainly
        // sitting there before any virtual time passes.
        pane.update(cx, |pane, cx| pane.clear(cx));
        std::thread::sleep(Duration::from_millis(400));

        // Three foreground-sized steps: 24ms of virtual time, less than one 33ms background
        // tick - a correctly-backgrounded pane must not have drained the echo yet.
        for _ in 0..3 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
        }
        let lines = pane.read_with(cx, |pane, _| pane.visible_text_lines());
        assert!(
            !lines.iter().any(|line| line.contains("^L")),
            "a background pane drained pty output in under one BACKGROUND_POLL_INTERVAL - \
             the poll loop is not reading the live foreground flag each tick"
        );

        // Past the background interval the echo must drain normally (bounded tick loop, as in
        // the ctrl-l test above, since exactly which tick isn't under test).
        let mut saw_caret_l = false;
        for _ in 0..50 {
            cx.background_executor
                .advance_clock(BACKGROUND_POLL_INTERVAL);
            cx.run_until_parked();
            let lines = pane.read_with(cx, |pane, _| pane.visible_text_lines());
            if lines.iter().any(|line| line.contains("^L")) {
                saw_caret_l = true;
                break;
            }
        }
        assert!(
            saw_caret_l,
            "the background cadence must still drain output - coarser, not never"
        );

        // And a promoted pane resumes draining (which tick is again not under test - only
        // that promotion doesn't strand it).
        pane.update(cx, |pane, cx| {
            pane.set_foreground(true);
            pane.clear(cx); // wipes the grid, sends a fresh Ctrl-L
        });
        std::thread::sleep(Duration::from_millis(400));
        let mut saw_caret_l_again = false;
        for _ in 0..50 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            let lines = pane.read_with(cx, |pane, _| pane.visible_text_lines());
            if lines.iter().any(|line| line.contains("^L")) {
                saw_caret_l_again = true;
                break;
            }
        }
        assert!(
            saw_caret_l_again,
            "a pane promoted back to foreground must keep draining output"
        );
    }

    #[test]
    fn a_foreground_pane_gets_the_full_frame_accurate_cadence() {
        assert_eq!(
            tick_cadence(true, false),
            (POLL_INTERVAL, MAX_BYTES_PER_TICK),
            "the visible pane must keep the measured single-agent throughput fix"
        );
    }

    #[test]
    fn a_background_pane_gets_a_strictly_coarser_interval_and_smaller_budget() {
        let (interval, budget) = tick_cadence(false, false);
        assert_eq!(interval, BACKGROUND_POLL_INTERVAL);
        assert_eq!(budget, BACKGROUND_MAX_BYTES_PER_TICK);
        // The relationships, not just the current literals: the whole point of the split is
        // that a background pane generates strictly less foreground-thread work per second
        // than the visible one (see BACKGROUND_POLL_INTERVAL's docs for the measured 25-pane
        // regression this bounds).
        assert!(interval > POLL_INTERVAL);
        assert!(budget < MAX_BYTES_PER_TICK);
    }

    #[test]
    fn eof_pending_forces_the_foreground_cadence_so_the_exit_grace_stays_ten_seconds() {
        // MAX_EOF_POLL_TICKS is derived from POLL_INTERVAL; if a background pane ticked its
        // EOF grace countdown at BACKGROUND_POLL_INTERVAL instead, the real ~10s
        // exit-confirmation window would silently stretch ~4x (see tick_cadence's docs).
        assert_eq!(
            tick_cadence(false, true),
            (POLL_INTERVAL, MAX_BYTES_PER_TICK)
        );
        assert_eq!(
            tick_cadence(true, true),
            (POLL_INTERVAL, MAX_BYTES_PER_TICK)
        );
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;
    use gpui::{px, size};

    /// A representative cell size, close to what `TerminalPane::cell_size` measures in
    /// practice - used by tests below that only care about `size_to_grid`'s arithmetic, not
    /// the exact cell size.
    fn test_cell_size() -> Size<Pixels> {
        size(px(APPROX_CELL_WIDTH_PX), px(ROW_LINE_HEIGHT_PX))
    }

    #[test]
    fn size_to_grid_derives_columns_and_rows_from_the_given_size_not_a_fixed_constant() {
        // A plausible centre-pane content width once Phase A's shell chrome (rail + panel +
        // borders) is subtracted from a 1440px window - roughly 820px, not the full 1440.
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
        // Regression guard for the Phase-A bug: deriving columns from the whole 1440px window
        // (ignoring shell chrome either side) computed ~205 columns; the real centre-pane
        // width is ~820-840px, ~118-120 columns. Both are "correct" `size_to_grid` outputs for
        // their inputs - the bug was `maybe_resize_pty` feeding it the wrong one. Pins the two
        // magnitudes apart so a regression back to `window.viewport_size()` gets caught here.
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
        // Regression guard for the Phase-C bug: `size_to_grid` used to compute columns/rows
        // from hardcoded cell-size constants, ignoring the pane's actual rendered cell size.
        // A bigger cell must produce fewer columns/rows for the same pixel area, with the
        // ratio matching exactly.
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
        // At a plausible centre-pane width (828px) and typical panel height (~800px): how many
        // rows the old guessed cell size (7.0 x 16.0) computed versus the line-height-corrected
        // one (7.0 x 19.0). Isolates the height half of the Phase-C bug - the width half is
        // that `cell_size`'s width now comes from a real font-metrics measurement instead of a
        // guess, which this pure function can't itself demonstrate.
        let old_guess = size(px(APPROX_CELL_WIDTH_PX), px(16.0));
        let new_real = size(px(APPROX_CELL_WIDTH_PX), px(ROW_LINE_HEIGHT_PX));
        let pane = size(px(828.0), px(800.0));

        let (old_rows, _) = size_to_grid(pane, old_guess);
        let (new_rows, _) = size_to_grid(pane, new_real);

        // The old, too-short line-height guess asked for more rows than the pane could
        // actually show without clipping (`800.0 / 16.0 = 50` vs. `800.0 / 19.0 = 42`) - a
        // ~19% over-request, not a rounding artifact.
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
        // The padding-box-vs-content-box bug this test pins: a measured pane of 844x713px at a
        // 7.2x19.0px cell size. Before the fix, `maybe_resize_pty` fed the raw padding-box
        // measurement straight into `size_to_grid`, computing 117 cols/37 rows - one more of
        // each than the real content box (844-16=828px wide, 713-16=697px tall) actually fits
        // (115 cols/36 rows), so the last column/row painted through `overflow_hidden()`.
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
        // look like "no exit status at all" (which `crate::rail::status::derive_status` would
        // read as `Status::Idle`) - it must resolve to a real, if synthetic, failed status.
        match eof_poll_decision(Ok(None), MAX_EOF_POLL_TICKS) {
            Some(status) => assert!(
                !status.success(),
                "giving up must never be reported as a successful exit"
            ),
            None => panic!("expected the tick cap to force a resolution"),
        }
    }

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
        // Regression test for the secondary-N-swallowed-by-a-focused-terminal bug: before the
        // fix, this fell through to the `key_char` branch and returned `Some(b"n")`, which
        // `handle_key_down` then wrote to the pty *and* called `stop_propagation()` on -
        // silently eating the app-level shortcut and typing a stray "n" into the agent.
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

    #[test]
    fn ctrl_p_maps_to_the_real_readline_previous_history_control_byte() {
        let modifiers = Modifiers {
            control: true,
            ..Default::default()
        };
        let ks = keystroke("p", modifiers);
        assert_eq!(
            keystroke_to_bytes(&ks),
            Some(vec![0x10]),
            "Ctrl+P must map to the real Ctrl+<letter> control code (0x10), the standard \
             readline 'previous history' byte every real shell relies on"
        );
    }

    #[test]
    fn ctrl_z_maps_to_the_real_sigtstp_control_byte() {
        let modifiers = Modifiers {
            control: true,
            ..Default::default()
        };
        let ks = keystroke("z", modifiers);
        assert_eq!(
            keystroke_to_bytes(&ks),
            Some(vec![0x1a]),
            "Ctrl+Z must map to the real Ctrl+<letter> control code (0x1a), the SIGTSTP \
             terminal-suspend byte essentially every interactive program relies on"
        );
    }

    #[test]
    fn shift_tab_sends_the_real_back_tab_sequence_distinct_from_plain_tab() {
        let plain = keystroke("tab", Modifiers::default());
        assert_eq!(
            keystroke_to_bytes(&plain),
            Some(b"\t".to_vec()),
            "plain Tab must still send the ordinary tab byte"
        );

        let shifted = keystroke(
            "tab",
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            keystroke_to_bytes(&shifted),
            Some(b"\x1b[Z".to_vec()),
            "Shift+Tab must send the standard CSI back-tab sequence (\\x1b[Z), not the plain \
             tab byte - this is what readline-based tools and Claude Code's own CLI listen for \
             to cycle their mode in the opposite direction"
        );

        assert_ne!(
            keystroke_to_bytes(&plain),
            keystroke_to_bytes(&shifted),
            "Tab and Shift+Tab must be distinguishable on the wire"
        );
    }

    #[test]
    fn ctrl_shift_tab_is_unaffected_by_the_back_tab_fix() {
        let ks = keystroke(
            "tab",
            Modifiers {
                shift: true,
                control: true,
                ..Default::default()
            },
        );
        assert_eq!(
            keystroke_to_bytes(&ks),
            Some(b"\t".to_vec()),
            "Ctrl+Shift+Tab must keep sending the plain tab byte, unchanged by the Shift+Tab \
             back-tab fix"
        );
    }
}

#[cfg(test)]
mod link_segment_tests {
    use super::*;

    fn link(start: usize, end: usize, path: &str) -> LinkMatch {
        LinkMatch {
            start,
            end,
            path: path.to_string(),
            line: None,
            column: None,
        }
    }

    #[test]
    fn a_row_with_no_links_is_a_single_plain_segment() {
        let segments = split_segments(10, &[]);
        assert_eq!(segments, vec![RowSegment::Plain { start: 0, end: 10 }]);
    }

    #[test]
    fn a_link_in_the_middle_splits_into_prefix_link_suffix_not_a_whole_line_style() {
        let links = [link(4, 19, "tests/upload.rs")];
        let segments = split_segments(20, &links);
        assert_eq!(
            segments,
            vec![
                RowSegment::Plain { start: 0, end: 4 },
                RowSegment::Link {
                    start: 4,
                    end: 19,
                    link: links[0].clone(),
                },
                RowSegment::Plain { start: 19, end: 20 },
            ]
        );
    }

    #[test]
    fn a_link_at_the_very_start_has_no_leading_plain_segment() {
        let links = [link(0, 5, "a.txt")];
        let segments = split_segments(8, &links);
        assert_eq!(
            segments,
            vec![
                RowSegment::Link {
                    start: 0,
                    end: 5,
                    link: links[0].clone(),
                },
                RowSegment::Plain { start: 5, end: 8 },
            ]
        );
    }

    #[test]
    fn a_link_at_the_very_end_has_no_trailing_plain_segment() {
        let links = [link(3, 8, "a.txt")];
        let segments = split_segments(8, &links);
        assert_eq!(
            segments,
            vec![
                RowSegment::Plain { start: 0, end: 3 },
                RowSegment::Link {
                    start: 3,
                    end: 8,
                    link: links[0].clone(),
                },
            ]
        );
    }

    #[test]
    fn multiple_links_each_get_their_own_span_with_plain_text_between_them() {
        let links = [link(2, 5, "a.rs"), link(9, 12, "b.rs")];
        let segments = split_segments(14, &links);
        assert_eq!(
            segments,
            vec![
                RowSegment::Plain { start: 0, end: 2 },
                RowSegment::Link {
                    start: 2,
                    end: 5,
                    link: links[0].clone(),
                },
                RowSegment::Plain { start: 5, end: 9 },
                RowSegment::Link {
                    start: 9,
                    end: 12,
                    link: links[1].clone(),
                },
                RowSegment::Plain { start: 12, end: 14 },
            ]
        );
    }

    #[test]
    fn a_link_covering_the_whole_row_is_a_single_link_segment() {
        let links = [link(0, 10, "src/main.rs")];
        let segments = split_segments(10, &links);
        assert_eq!(
            segments,
            vec![RowSegment::Link {
                start: 0,
                end: 10,
                link: links[0].clone(),
            }]
        );
    }
}

#[cfg(test)]
mod clear_pty_signal_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn clear_with_a_live_session_sends_a_real_ctrl_l_the_pty_echoes_back(cx: &mut TestAppContext) {
        let pane = cx.new(|cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", Vec::new(), std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();

        pane.update(cx, |pane, cx| pane.clear(cx));

        // The real pty reader thread and the real `cat` child it feeds run entirely outside
        // GPUI's deterministic scheduler, so `advance_clock` alone (a purely *simulated* clock)
        // grants them zero actual wall-clock scheduling time. Standalone, with an otherwise idle
        // CPU, the real echo lands within real microseconds and a loop that only ever advances
        // the virtual clock never notices it's racing ahead of real time - but under real
        // full-suite parallel load (dozens of other tests' own real subprocesses contending for
        // the same cores) the OS can genuinely take real milliseconds to schedule this thread,
        // and a loop with no real-time floor at all can burn through every check before that
        // happens. A real `std::thread::sleep` between checks (bounded by a real deadline, not a
        // fixed tick count) gives it a genuine chance, mirroring this same file's own
        // `a_background_pane_drains_on_the_background_interval_read_fresh_each_tick`'s upfront
        // `std::thread::sleep(Duration::from_millis(400))` for the identical Ctrl-L/ECHOCTL
        // round trip.
        let mut saw_caret_l = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        loop {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            let lines = pane.read_with(cx, |pane, _| pane.visible_text_lines());
            if lines.iter().any(|line| line.contains("^L")) {
                saw_caret_l = true;
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            saw_caret_l,
            "expected the real pty's own echo of the Ctrl-L byte clear() sends to eventually \
             appear in the grid - this is what actually proves the byte reached the pty, not \
             just that write_input() was called"
        );
    }
}

/// GitHub issue #239's structural status signal, end to end through a **real pty**: a real child
/// process writes real escape sequences into a real pty, this pane's real poll loop reads them,
/// and the pane's accessors report what was parsed. Nothing here is injected or simulated - the
/// escape bytes are produced by a real `printf`, exactly as a real agent CLI produces them.
#[cfg(test)]
mod terminal_signal_tests {
    use super::*;
    use crate::rail::title_signal::{classify_title, TitleSignal};
    use crate::terminal::osc::{Progress, ProgressState};
    use gpui::TestAppContext;

    /// Spawns a real `sh` that writes `script`'s escape sequences and then blocks, so the
    /// process is still alive (and the pane still `is_running`) while the assertions run - the
    /// same real-pty pattern this module's other tests use, and `sh` rather than bare `printf`
    /// so the sequences and the sleep are one child process.
    fn pane_emitting(cx: &mut TestAppContext, script: &str) -> gpui::Entity<TerminalPane> {
        cx.new(|cx| {
            TerminalPane::new(
                TerminalSpec::command(
                    "sh",
                    vec!["-c".to_string(), format!("{script}; sleep 60")],
                    std::env::temp_dir(),
                ),
                ROW_FONT_SIZE_PX,
                cx,
            )
        })
    }

    /// Drives the pane's real poll loop until `done` reports true, or gives up. The pty write
    /// itself takes real wall time (a real process, real fd) while the *drain* only happens on
    /// poll ticks, which the test executor's clock drives - hence both a real sleep and the
    /// virtual-clock loop, matching `cadence_tests`' own approach.
    fn poll_until(
        cx: &mut TestAppContext,
        pane: &gpui::Entity<TerminalPane>,
        done: impl Fn(&TerminalPane) -> bool,
    ) -> bool {
        cx.run_until_parked();
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(50));
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            if pane.read_with(cx, |pane, _| done(pane)) {
                return true;
            }
        }
        false
    }

    #[gpui::test]
    fn a_real_pty_process_setting_its_title_is_captured_and_classified(cx: &mut TestAppContext) {
        // Byte for byte what Claude Code writes while it is working - see
        // `crate::rail::title_signal`'s module docs for the live capture this came from.
        // The glyph goes in as a literal UTF-8 character rather than a `printf` escape: `\033`
        // and `\007` are POSIX octal escapes every `printf` supports, but `\u` is not portable.
        let pane = pane_emitting(cx, "printf '\\033]0;\u{25d0} Claude Code\\007'");
        assert!(
            poll_until(cx, &pane, |pane| pane.title().is_some()),
            "the pane never captured a title from a real pty"
        );

        let title = pane.read_with(cx, |pane, _| pane.title().map(str::to_string));
        assert_eq!(title.as_deref(), Some("\u{25d0} Claude Code"));
        assert_eq!(
            classify_title(title.as_deref().unwrap()),
            TitleSignal::Busy,
            "a real agent CLI's real working title must classify as Busy end to end"
        );

        pane.update(cx, |pane, cx| pane.shutdown(cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_real_osc_9_notification_from_a_pty_process_latches_an_attention_ping(
        cx: &mut TestAppContext,
    ) {
        let pane = pane_emitting(cx, "printf '\\033]9;Agent needs your input\\007'");
        assert!(
            !pane.read_with(cx, |pane, _| pane.has_pending_attention_ping()),
            "sanity check: nothing pinged before the process wrote anything"
        );
        assert!(
            poll_until(cx, &pane, |pane| pane.has_pending_attention_ping()),
            "a real OSC 9 notification off a real pty never reached the pane"
        );
        // The latch is state, not a one-shot: reading it twice must not consume it, or the
        // render path would see "needs input" for exactly one frame. See
        // `TerminalPane::attention_ping_at`'s field docs.
        assert!(
            pane.read_with(cx, |pane, _| pane.has_pending_attention_ping()),
            "the pane's ping must survive being read"
        );

        pane.update(cx, |pane, cx| pane.shutdown(cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_real_osc_777_notify_from_a_pty_process_also_pings(cx: &mut TestAppContext) {
        let pane = pane_emitting(cx, "printf '\\033]777;notify;Gemini;your turn\\007'");
        assert!(
            poll_until(cx, &pane, |pane| pane.has_pending_attention_ping()),
            "a real OSC 777 notify off a real pty never reached the pane"
        );

        pane.update(cx, |pane, cx| pane.shutdown(cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_real_osc_9_4_progress_report_from_a_pty_process_is_parsed(cx: &mut TestAppContext) {
        let pane = pane_emitting(cx, "printf '\\033]9;4;1;73\\007'");
        assert!(
            poll_until(cx, &pane, |pane| pane.progress().is_some()),
            "a real OSC 9;4 progress report off a real pty never reached the pane"
        );
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.progress()),
            Some(Progress {
                state: ProgressState::Normal,
                percent: Some(73)
            })
        );
        assert!(
            !pane.read_with(cx, |pane, _| pane.has_pending_attention_ping()),
            "a progress report is not a notification - OSC 9;4 must not ping for attention"
        );

        pane.update(cx, |pane, cx| pane.shutdown(cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_pty_process_that_says_nothing_reports_no_signal_at_all(cx: &mut TestAppContext) {
        // The honest default, and the one every non-agent process hits: a `cat` sitting on a
        // real pty has no title, no ping and no progress - not an invented one.
        let pane = cx.new(|cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", Vec::new(), std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();
        for _ in 0..5 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
        }
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.title(), None);
            assert!(!pane.has_pending_attention_ping());
            assert_eq!(pane.progress(), None);
        });

        pane.update(cx, |pane, cx| pane.shutdown(cx));
        cx.run_until_parked();
    }
}

/// GitHub issue #158's pixel-to-cell arithmetic - the half of mouse text selection that has
/// nothing to do with GPUI. A wrong mapping here is invisible in a screenshot but silently
/// copies the wrong characters, so it is pinned exactly.
#[cfg(test)]
mod cell_position_tests {
    use super::*;
    use gpui::{point, size};

    /// A 10x20px cell grid whose origin is at (100, 200) - deliberately not (0, 0), so an
    /// implementation that forgot to subtract the origin fails instead of accidentally passing.
    fn at(x: f32, y: f32) -> CellPosition {
        cell_position_in_grid(
            point(px(100.0 + x), px(200.0 + y)),
            point(px(100.0), px(200.0)),
            size(px(10.0), px(20.0)),
            24, // rows
            80, // cols
        )
    }

    #[test]
    fn the_top_left_pixel_is_row_zero_column_zero() {
        assert_eq!(
            at(0.0, 0.0),
            CellPosition {
                row: 0,
                column: 0,
                side: CellSide::Left
            }
        );
    }

    #[test]
    fn a_position_resolves_to_the_cell_containing_it() {
        // x = 34px -> column 3 (30..40), 0.4 of the way in -> left half.
        // y = 55px -> row 2 (40..60).
        assert_eq!(
            at(34.0, 55.0),
            CellPosition {
                row: 2,
                column: 3,
                side: CellSide::Left
            }
        );
    }

    #[test]
    fn the_right_half_of_a_cell_reports_the_right_side() {
        assert_eq!(at(35.0, 0.0).side, CellSide::Right);
        assert_eq!(at(39.9, 0.0).side, CellSide::Right);
        assert_eq!(at(34.9, 0.0).side, CellSide::Left);
        assert_eq!(
            at(35.0, 0.0).column,
            3,
            "the column is unchanged by the side"
        );
    }

    #[test]
    fn a_drag_past_the_right_edge_clamps_to_the_last_column_inclusively() {
        let position = at(10_000.0, 0.0);
        assert_eq!(position.column, 79);
        assert_eq!(
            position.side,
            CellSide::Right,
            "clamping to the *left* of the last column would silently drop that column from \
             the selection"
        );
    }

    #[test]
    fn a_drag_past_the_bottom_clamps_to_the_last_row() {
        assert_eq!(at(0.0, 10_000.0).row, 23);
    }

    #[test]
    fn a_position_above_and_left_of_the_origin_clamps_to_the_first_cell() {
        let position = cell_position_in_grid(
            point(px(0.0), px(0.0)),
            point(px(100.0), px(200.0)),
            size(px(10.0), px(20.0)),
            24,
            80,
        );
        assert_eq!(position.row, 0);
        assert_eq!(position.column, 0);
    }

    #[test]
    fn a_degenerate_zero_cell_size_does_not_produce_a_garbage_index() {
        let position = cell_position_in_grid(
            point(px(50.0), px(50.0)),
            point(px(0.0), px(0.0)),
            size(px(0.0), px(0.0)),
            24,
            80,
        );
        assert!(position.row < 24);
        assert!(position.column < 80);
    }
}

/// GitHub issue #158's paste framing. Writing the raw clipboard string to the pty is wrong in
/// both modes for different reasons - see [`paste_payload`]'s own docs.
#[cfg(test)]
mod paste_payload_tests {
    use super::*;

    #[test]
    fn without_bracketed_paste_newlines_become_carriage_returns() {
        assert_eq!(paste_payload("one\ntwo", false), "one\rtwo");
        assert_eq!(
            paste_payload("one\r\ntwo", false),
            "one\rtwo",
            "a CRLF clipboard payload (Windows, or copied out of a browser) must not send two \
             separate line endings"
        );
        assert_eq!(paste_payload("plain", false), "plain");
    }

    #[test]
    fn with_bracketed_paste_the_text_is_wrapped_and_left_otherwise_intact() {
        assert_eq!(
            paste_payload("one\ntwo", true),
            "\x1b[200~one\ntwo\x1b[201~",
            "inside brackets the receiving program decides what a newline means - rewriting it \
             here is what makes a multi-line paste auto-execute"
        );
    }

    #[test]
    fn a_pasted_escape_cannot_close_the_bracket_early() {
        let payload = paste_payload("safe\x1b[201~rm -rf /", true);
        assert_eq!(payload.matches("\x1b[201~").count(), 1);
        assert!(payload.ends_with("\x1b[201~"));
    }
}

/// GitHub issue #158, end to end at the pane level: a real selection produces a real OS
/// clipboard write, and a real clipboard read produces real bytes on a real pty.
#[cfg(test)]
mod clipboard_tests {
    use super::*;
    use gpui::TestAppContext;

    fn new_pane(cx: &mut TestAppContext, program: &str) -> gpui::Entity<TerminalPane> {
        cx.new(|cx| {
            TerminalPane::new(
                TerminalSpec::command(program, Vec::new(), std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        })
    }

    #[gpui::test]
    fn copying_a_real_selection_writes_it_to_the_real_system_clipboard(cx: &mut TestAppContext) {
        let pane = new_pane(cx, "cat");
        cx.run_until_parked();

        cx.update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into())));

        let copied = pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(b"hello world", cx);
            pane.grid.start_selection(CellPosition {
                row: 0,
                column: 6,
                side: CellSide::Left,
            });
            pane.grid.update_selection(CellPosition {
                row: 0,
                column: 10,
                side: CellSide::Right,
            });
            pane.copy_selection(cx)
        });

        assert!(
            copied,
            "a real, non-empty selection must report a real copy"
        );
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("world"));
    }

    #[gpui::test]
    fn copying_with_no_selection_leaves_the_clipboard_untouched(cx: &mut TestAppContext) {
        let pane = new_pane(cx, "cat");
        cx.run_until_parked();

        cx.update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("keep me".into())));

        let copied = pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(b"hello world", cx);
            pane.copy_selection(cx)
        });

        assert!(!copied);
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("keep me"));
    }

    #[gpui::test]
    fn pasting_writes_the_real_clipboard_text_to_the_real_pty(cx: &mut TestAppContext) {
        let pane = new_pane(cx, "cat");
        cx.run_until_parked();

        cx.update(|cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("ade-pasted-text".into()))
        });
        let pasted = pane.update(cx, |pane, cx| pane.paste_from_clipboard(cx));
        assert!(pasted, "a live session must accept a real paste");

        let mut saw_pasted_text = false;
        for _ in 0..50 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            let lines = pane.read_with(cx, |pane, _| pane.visible_text_lines());
            if lines.iter().any(|line| line.contains("ade-pasted-text")) {
                saw_pasted_text = true;
                break;
            }
        }
        assert!(
            saw_pasted_text,
            "expected the real pty's own echo of the pasted bytes to appear in the grid"
        );
    }

    #[gpui::test]
    fn pasting_an_empty_clipboard_writes_nothing(cx: &mut TestAppContext) {
        let pane = new_pane(cx, "cat");
        cx.run_until_parked();

        cx.update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new())));
        let pasted = pane.update(cx, |pane, cx| pane.paste_from_clipboard(cx));
        assert!(!pasted);
    }
}

/// GitHub issue #288's delivery mechanism, against a real pty and a real child process.
#[cfg(test)]
mod send_prompt_tests {
    use super::*;
    use gpui::TestAppContext;

    /// A real child on a real pty. `program`/`args` are spawned as-is.
    fn new_pane(
        cx: &mut TestAppContext,
        program: &str,
        args: Vec<String>,
    ) -> gpui::Entity<TerminalPane> {
        cx.new(|cx| {
            TerminalPane::new(
                TerminalSpec::command(program, args, std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        })
    }

    /// Pumps the real poll loop until `ready`, or gives up after a real wall-clock deadline.
    /// Mixes virtual-clock advance with a real `sleep`, exactly as
    /// `crate::work_surface::render`'s own `wait_for_real_pty_output` does and for the same
    /// reason: the pty reader is a real OS thread, so a virtual-clock-only loop races it.
    fn pump_until(
        cx: &mut TestAppContext,
        mut ready: impl FnMut(&mut TestAppContext) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            if ready(cx) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[gpui::test]
    fn a_multi_line_batch_reaches_a_real_pty_whose_child_has_bracketed_paste_on(
        cx: &mut TestAppContext,
    ) {
        // `printf` sets `DECSET 2004` on its own output, which is how the grid - and therefore
        // `bracketed_paste_enabled` - learns about it. Nothing here is simulated: the mode really
        // arrives over the pty from a real child, then `cat` echoes whatever is written back.
        let pane = new_pane(
            cx,
            "sh",
            vec!["-c".to_string(), "printf '\\033[?2004h'; cat".to_string()],
        );
        cx.run_until_parked();
        assert!(
            pump_until(cx, |cx| pane
                .read_with(cx, |pane, _| pane.bracketed_paste_enabled())),
            "precondition: the real child must have turned bracketed paste on"
        );

        let sent = pane.update(cx, |pane, cx| {
            pane.send_prompt(
                "Review notes on src/api/users.rs \u{2014} 1 note, one prompt, line-anchored.\n\
                 line 13: ade-note-tenant-id",
                cx,
            )
        });
        assert!(
            sent,
            "a live session with bracketed paste on must accept it"
        );

        assert!(
            pump_until(cx, |cx| {
                pane.read_with(cx, |pane, _| {
                    pane.visible_text_lines()
                        .iter()
                        .any(|line| line.contains("ade-note-tenant-id"))
                })
            }),
            "the real pty's own echo of the batched prompt must appear in the grid - this is \
             round-tripped evidence the note text genuinely left the app, not an assertion that \
             `write_input` was called"
        );
    }

    #[gpui::test]
    fn a_multi_line_batch_is_refused_when_the_child_has_bracketed_paste_off(
        cx: &mut TestAppContext,
    ) {
        let pane = new_pane(cx, "cat", Vec::new());
        cx.run_until_parked();
        assert!(
            !pane.read_with(cx, |pane, _| pane.bracketed_paste_enabled()),
            "precondition: a plain `cat` never turns bracketed paste on"
        );

        let sent = pane.update(cx, |pane, cx| {
            pane.send_prompt("line 5: ade-refused-one\nline 9: ade-refused-two", cx)
        });
        assert!(
            !sent,
            "a payload that could split must be refused, not sent"
        );

        // Pumped for real, so "nothing arrived" is a measured fact rather than an assumption
        // about timing.
        for _ in 0..40 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
        }
        pane.read_with(cx, |pane, _| {
            assert!(
                !pane
                    .visible_text_lines()
                    .iter()
                    .any(|line| line.contains("ade-refused")),
                "not one byte of a refused batch may reach the child"
            );
            assert!(
                pane.spawn_error.is_some(),
                "and the refusal must be said out loud on the pane, not swallowed"
            );
        });
    }

    #[gpui::test]
    fn the_single_line_form_is_delivered_to_a_child_with_bracketed_paste_off(
        cx: &mut TestAppContext,
    ) {
        let pane = new_pane(cx, "cat", Vec::new());
        cx.run_until_parked();

        let sent = pane.update(cx, |pane, cx| {
            pane.send_prompt("line 5: ade-flat-one \u{b7} line 9: ade-flat-two", cx)
        });
        assert!(sent);
        assert!(
            pump_until(cx, |cx| {
                pane.read_with(cx, |pane, _| {
                    pane.visible_text_lines()
                        .iter()
                        .any(|line| line.contains("ade-flat-two"))
                })
            }),
            "the flat form must really reach the child"
        );
    }

    #[gpui::test]
    fn an_empty_prompt_writes_nothing(cx: &mut TestAppContext) {
        let pane = new_pane(cx, "cat", Vec::new());
        cx.run_until_parked();
        assert!(!pane.update(cx, |pane, cx| pane.send_prompt("", cx)));
    }
}

/// GitHub issue #158's mouse half, end to end through real GPUI event dispatch: before this,
/// `TerminalPane` registered no mouse-down/move/up handlers at all, so no amount of dragging
/// produced a selection and "copy" would have had nothing to copy even with a binding in place.
#[cfg(test)]
mod mouse_selection_tests {
    use super::*;
    use gpui::{point, Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext};

    /// Opens a real window on a `cat`-backed pane, puts `"hello world"` on row 0, and returns
    /// the painted geometry a drag position is computed from.
    fn painted_pane(
        cx: &mut TestAppContext,
    ) -> (
        Entity<TerminalPane>,
        &mut VisualTestContext,
        Bounds<Pixels>,
        Size<Pixels>,
    ) {
        painted_pane_showing(cx, b"hello world")
    }

    /// [`painted_pane`] with caller-chosen row-0 content - used by the wide-character drag
    /// coverage (GitHub issue #211), which needs real CJK on screen.
    fn painted_pane_showing<'a>(
        cx: &'a mut TestAppContext,
        row_zero: &[u8],
    ) -> (
        Entity<TerminalPane>,
        &'a mut VisualTestContext,
        Bounds<Pixels>,
        Size<Pixels>,
    ) {
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", Vec::new(), std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();

        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(row_zero, cx);
        });
        cx.run_until_parked();

        let (bounds, cell_size) = pane.update_in(cx, |pane, window, _cx| {
            (
                pane.content_bounds_for_test(),
                pane.cell_size_for_test(window),
            )
        });
        let bounds = bounds.expect("the pane must have painted at least once");
        (pane, cx, bounds, cell_size)
    }

    /// The centre of cell `(row, column)` in window space.
    fn cell_centre(
        bounds: Bounds<Pixels>,
        cell_size: Size<Pixels>,
        row: usize,
        column: usize,
    ) -> gpui::Point<Pixels> {
        point(
            bounds.origin.x + px(PANE_PADDING_PX) + cell_size.width * (column as f32 + 0.5),
            bounds.origin.y + px(PANE_PADDING_PX) + cell_size.height * (row as f32 + 0.5),
        )
    }

    #[gpui::test]
    fn a_real_mouse_drag_selects_the_text_it_dragged_over(cx: &mut TestAppContext) {
        let (pane, cx, bounds, cell_size) = painted_pane(cx);

        // "hello world": drag from the left edge of 'w' (column 6) to the right edge of 'd'
        // (column 10). Starting a touch left of centre and ending a touch right of centre is
        // what a real user's drag looks like, and is what makes both boundary cells included.
        let start = point(
            bounds.origin.x + px(PANE_PADDING_PX) + cell_size.width * 6.1,
            cell_centre(bounds, cell_size, 0, 6).y,
        );
        let end = point(
            bounds.origin.x + px(PANE_PADDING_PX) + cell_size.width * 10.9,
            cell_centre(bounds, cell_size, 0, 10).y,
        );

        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.selected_text_for_test())
                .as_deref(),
            Some("world"),
            "a real left-drag across the grid must produce a real alacritty_terminal selection"
        );
    }

    #[gpui::test]
    fn a_plain_click_clears_a_previous_selection(cx: &mut TestAppContext) {
        let (pane, cx, bounds, cell_size) = painted_pane(cx);

        let start = cell_centre(bounds, cell_size, 0, 0);
        let end = cell_centre(bounds, cell_size, 0, 4);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert!(
            pane.read_with(cx, |pane, _| pane.selected_text_for_test())
                .is_some(),
            "sanity check: the drag must have selected something to then clear"
        );

        cx.simulate_click(cell_centre(bounds, cell_size, 2, 3), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.selected_text_for_test()),
            None
        );
    }

    #[gpui::test]
    fn hovering_without_a_button_held_selects_nothing(cx: &mut TestAppContext) {
        let (pane, cx, bounds, cell_size) = painted_pane(cx);

        cx.simulate_mouse_move(
            cell_centre(bounds, cell_size, 0, 0),
            None,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            cell_centre(bounds, cell_size, 0, 10),
            None,
            Modifiers::none(),
        );
        cx.run_until_parked();

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.selected_text_for_test()),
            None
        );
    }

    #[gpui::test]
    fn a_drag_marks_the_covered_cells_selected_for_the_renderer(cx: &mut TestAppContext) {
        let (pane, cx, bounds, cell_size) = painted_pane(cx);

        let start = point(
            bounds.origin.x + px(PANE_PADDING_PX) + cell_size.width * 0.1,
            cell_centre(bounds, cell_size, 0, 0).y,
        );
        let end = point(
            bounds.origin.x + px(PANE_PADDING_PX) + cell_size.width * 4.9,
            cell_centre(bounds, cell_size, 0, 4).y,
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        let flagged = pane.read_with(cx, |pane, _| {
            pane.grid.visible_rows(&TerminalPalette::default())[0]
                .iter()
                .filter(|cell| cell.selected)
                .map(|cell| cell.c)
                .collect::<String>()
        });
        assert_eq!(flagged, "hello");
    }

    #[gpui::test]
    fn a_drag_after_two_cjk_characters_still_hits_the_columns_it_looks_like_it_hits(
        cx: &mut TestAppContext,
    ) {
        let (pane, cx, bounds, cell_size) = painted_pane_showing(cx, "你好world".as_bytes());

        let start = point(
            bounds.origin.x + px(PANE_PADDING_PX) + cell_size.width * 4.1,
            cell_centre(bounds, cell_size, 0, 4).y,
        );
        let end = point(
            bounds.origin.x + px(PANE_PADDING_PX) + cell_size.width * 8.9,
            cell_centre(bounds, cell_size, 0, 8).y,
        );

        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.selected_text_for_test())
                .as_deref(),
            Some("world")
        );
    }

    #[gpui::test]
    fn a_drag_over_cjk_characters_copies_them_once_each(cx: &mut TestAppContext) {
        let (pane, cx, bounds, cell_size) = painted_pane_showing(cx, "你好world".as_bytes());

        let start = point(
            bounds.origin.x + px(PANE_PADDING_PX) + cell_size.width * 0.1,
            cell_centre(bounds, cell_size, 0, 0).y,
        );
        let end = point(
            bounds.origin.x + px(PANE_PADDING_PX) + cell_size.width * 3.9,
            cell_centre(bounds, cell_size, 0, 3).y,
        );

        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.selected_text_for_test())
                .as_deref(),
            Some("你好")
        );
    }
}

/// GitHub issue #211's *input* half - real UTF-8 going **into** the pty rather than coming out of
/// it.
#[cfg(test)]
mod utf8_input_tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext};

    fn typed(text: &str) -> Keystroke {
        Keystroke {
            key: text.to_string(),
            key_char: Some(text.to_string()),
            modifiers: Modifiers::none(),
        }
    }

    #[test]
    fn a_multi_byte_character_is_forwarded_as_its_exact_utf8_bytes() {
        assert_eq!(
            keystroke_to_bytes(&typed("好")),
            Some("好".as_bytes().to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&typed("好")),
            Some(vec![0xe5, 0xa5, 0xbd]),
            "spelled out, so a change that started re-encoding this fails visibly"
        );
    }

    #[test]
    fn a_multi_codepoint_grapheme_cluster_is_forwarded_whole() {
        let family = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(family.chars().count(), 5, "sanity check on the fixture");
        assert_eq!(
            keystroke_to_bytes(&typed(family)),
            Some(family.as_bytes().to_vec())
        );
    }

    #[test]
    fn paste_framing_leaves_multi_byte_text_byte_for_byte_intact() {
        let text = "日本語 🎉";
        assert_eq!(paste_payload(text, false), text);
        assert_eq!(
            paste_payload(text, true),
            format!("\x1b[200~{text}\x1b[201~")
        );
    }

    #[gpui::test]
    fn typing_a_cjk_character_round_trips_through_a_real_pty_as_a_wide_cell(
        cx: &mut TestAppContext,
    ) {
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", Vec::new(), std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();

        pane.update_in(cx, |pane, window, cx| {
            for text in ["日", "本", "🎉"] {
                pane.handle_key_down(
                    &KeyDownEvent {
                        keystroke: typed(text),
                        is_held: false,
                        prefer_character_input: true,
                    },
                    window,
                    cx,
                );
            }
        });

        let mut echoed = None;
        for _ in 0..100 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            let row = pane.read_with(cx, |pane, _| pane.painted_rows_for_test().remove(0));
            let text: String = row
                .iter()
                .filter(|cell| cell.width != CellWidth::Spacer)
                .map(|cell| cell.c)
                .collect();
            if text.trim_end() == "日本🎉" {
                echoed = Some(row);
                break;
            }
        }

        let row = echoed.expect("expected the real pty's echo of the typed UTF-8 in the grid");
        assert_eq!(
            row.iter()
                .take(6)
                .map(|cell| cell.width)
                .collect::<Vec<_>>(),
            vec![
                CellWidth::Wide,
                CellWidth::Spacer,
                CellWidth::Wide,
                CellWidth::Spacer,
                CellWidth::Wide,
                CellWidth::Spacer,
            ]
        );
    }
}

/// GitHub issue #211's rendering half. Before this, `render_row` painted every grid cell as one
/// glyph of ordinary monospace text, so a double-width character reached the screen as its own
/// (roughly two-advance) glyph *plus* the blank spacer cell `alacritty_terminal` writes after it -
/// three columns of paint for two columns of grid, and every character after it on the row shifted
/// right.
#[cfg(test)]
mod wide_char_render_tests {
    use super::*;

    /// Row 0 of a real grid after parsing `bytes`.
    fn row_zero(bytes: &str, cols: u16) -> Vec<GridCell> {
        let mut grid = TerminalGrid::new(2, cols);
        grid.append_bytes(bytes.as_bytes());
        grid.visible_rows(&TerminalPalette::default())
            .into_iter()
            .next()
            .expect("a two-row grid always has a row 0")
    }

    #[test]
    fn painted_columns_always_sum_to_the_grid_column_count() {
        for bytes in [
            "plain ascii",
            "你好world",
            "🎉🎉🎉",
            "a你b好c",
            "abcd好", // wraps: the last column holds a real blank, not a spacer
        ] {
            let row = row_zero(bytes, 20);
            let painted: usize = row.iter().map(|cell| cell.width.columns()).sum();
            assert_eq!(
                painted,
                row.len(),
                "row {bytes:?} paints {painted} columns for {} grid columns",
                row.len()
            );
        }
    }

    #[test]
    fn a_spacer_cell_contributes_no_glyph_of_its_own() {
        let runs = row_runs(&row_zero("你好world", 20));
        let painted: String = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(
            painted.trim_end(),
            "你好world",
            "the emulator's own padding spaces must never reach the screen"
        );
    }

    #[test]
    fn each_wide_character_is_its_own_two_column_run() {
        let runs = row_runs(&row_zero("你好world", 20));

        assert_eq!(runs[0].text, "你");
        assert_eq!(runs[0].columns, 2);
        assert_eq!(runs[0].style.width, CellWidth::Wide);
        assert_eq!(runs[1].text, "好");
        assert_eq!(runs[1].columns, 2);

        assert_eq!(runs[2].text.trim_end(), "world");
        assert_eq!(runs[2].style.width, CellWidth::Narrow);
        assert_eq!(
            runs[2].columns,
            runs[2].text.chars().count(),
            "a narrow run covers exactly one column per character"
        );
    }

    #[test]
    fn only_a_wide_run_gets_an_explicit_pixel_width() {
        let runs = row_runs(&row_zero("你好world", 20));
        assert_eq!(wide_run_width(&runs[0], px(8.0)), Some(px(16.0)));
        assert_eq!(wide_run_width(&runs[2], px(8.0)), None);
    }

    #[test]
    fn a_wide_run_never_merges_into_the_narrow_text_around_it() {
        let runs = row_runs(&row_zero("a好b", 20));
        let shapes: Vec<(&str, usize)> = runs
            .iter()
            .map(|run| (run.text.trim_end(), run.columns))
            .collect();
        assert_eq!(shapes[0], ("a", 1));
        assert_eq!(shapes[1], ("好", 2));
        assert_eq!(&shapes[2].0[..1], "b");
    }

    #[test]
    fn a_link_containing_wide_characters_is_still_detected_as_one_link() {
        let runs = row_runs(&row_zero("see /tmp/日本/main.rs:12 ok", 40));
        let linked: Vec<&RowRun> = runs.iter().filter(|run| run.link.is_some()).collect();

        let path = linked
            .first()
            .and_then(|run| run.link.as_ref())
            .map(|link| link.path.clone());
        assert_eq!(path.as_deref(), Some("/tmp/日本/main.rs"));
        assert_eq!(
            linked
                .first()
                .and_then(|run| run.link.as_ref())
                .and_then(|link| link.line),
            Some(12)
        );

        let link_text: String = linked.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(link_text, "/tmp/日本/main.rs:12");
    }

    #[test]
    fn a_link_after_a_wide_character_still_lands_on_the_right_characters() {
        let runs = row_runs(&row_zero("好 src/main.rs done", 40));
        let linked: Vec<&RowRun> = runs.iter().filter(|run| run.link.is_some()).collect();
        let link_text: String = linked.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(link_text, "src/main.rs");

        let painted: String = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(painted.trim_end(), "好 src/main.rs done");
    }

    #[test]
    fn the_selection_flag_survives_onto_the_wide_characters_it_covers() {
        let mut grid = TerminalGrid::new(2, 20);
        grid.append_bytes("你好世界".as_bytes());
        grid.start_selection(CellPosition {
            row: 0,
            column: 0,
            side: CellSide::Left,
        });
        grid.update_selection(CellPosition {
            row: 0,
            column: 3,
            side: CellSide::Right,
        });
        let row = grid
            .visible_rows(&TerminalPalette::default())
            .into_iter()
            .next()
            .expect("a two-row grid always has a row 0");

        let runs = row_runs(&row);
        let selected: String = runs
            .iter()
            .filter(|run| run.style.selected)
            .map(|run| run.text.as_str())
            .collect();
        assert_eq!(selected, "你好");
        assert!(runs.iter().all(|run| run.style.width != CellWidth::Wide
            || run.columns == 2 && run.text.chars().count() == 1));
    }

    #[gpui::test]
    fn the_plain_text_view_of_the_grid_drops_the_padding_spaces(cx: &mut gpui::TestAppContext) {
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", Vec::new(), std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();
        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test("完了 🎉".as_bytes(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.visible_text_lines())[0],
            "完了 🎉"
        );
    }
}

/// GitHub issue #208, end to end through a real painted GPUI window: the integrated terminal's own
/// rendered colours must follow the selected theme.
#[cfg(test)]
mod terminal_theme_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::rc::Rc;

    /// Restores the Jerry Dark identity (no palette installed) on drop, so a test that installs a
    /// theme - or panics half way through one - can't leak it into any other test on this thread.
    struct ResetThemeOnDrop;

    impl Drop for ResetThemeOnDrop {
        fn drop(&mut self) {
            theme::set_current_theme(None);
        }
    }

    /// Installs a real bundled theme exactly the way selecting its card does: compiled from its own
    /// checked-in `assets/themes/*.toml` file through the real `base` chain
    /// (`crate::settings::custom_theme::compile_palette_by_name`, which is what
    /// `AdeApp::apply_theme_selection` calls), not a palette synthesized for the test.
    fn with_bundled_theme(name: &str) -> ResetThemeOnDrop {
        let palette = crate::settings::custom_theme::compile_palette_by_name(name, &[])
            .expect("a bundled theme must compile")
            .expect("a bundled non-Jerry-Dark theme has real overrides");
        theme::set_current_theme(Some(Rc::new(palette)));
        ResetThemeOnDrop
    }

    /// That bundled theme's own value for one key, straight out of its compiled palette.
    fn bundled_rgb(name: &str, key: &str) -> (u8, u8, u8) {
        let palette = crate::settings::custom_theme::compile_palette_by_name(name, &[])
            .expect("a bundled theme must compile")
            .expect("a bundled non-Jerry-Dark theme has real overrides");
        let color = *palette.get(key).expect("a real registered key");
        let channel = |component: f32| (component.clamp(0.0, 1.0) * 255.0).round() as u8;
        (channel(color.r), channel(color.g), channel(color.b))
    }

    /// Opens a real window on a `cat`-backed pane (`cat` writes nothing of its own, so the grid
    /// holds exactly the injected bytes) and paints it at least once.
    fn painted_pane<'a>(
        cx: &'a mut TestAppContext,
        bytes: &[u8],
    ) -> (Entity<TerminalPane>, &'a mut VisualTestContext) {
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", Vec::new(), std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();
        pane.update(cx, |pane, cx| pane.inject_bytes_for_test(bytes, cx));
        cx.run_until_parked();
        (pane, cx)
    }

    #[test]
    fn the_pure_grid_default_palette_is_exactly_jerry_darks_own() {
        assert!(
            theme::current_theme_palette().is_none(),
            "the real Jerry Dark identity case - no palette installed"
        );
        assert_eq!(
            theme_terminal_palette(),
            TerminalPalette::default(),
            "crate::terminal::grid's TerminalPalette::default() has drifted from \
             crate::theme::terminal's own compiled defaults - retune one and you must retune both"
        );
    }

    #[gpui::test]
    fn switching_to_the_light_theme_really_repaints_the_terminal_light(cx: &mut TestAppContext) {
        let (pane, cx) = painted_pane(cx, b"hello world");

        let dark = pane
            .read_with(cx, |pane, _| pane.last_painted_palette_for_test())
            .expect("the pane must have painted at least once");
        assert_eq!(
            dark,
            TerminalPalette::default(),
            "with no theme installed the pane must paint Jerry Dark's own palette"
        );

        let _theme = with_bundled_theme("Paper");
        pane.update(cx, |_pane, cx| cx.notify());
        cx.run_until_parked();

        let light = pane
            .read_with(cx, |pane, _| pane.last_painted_palette_for_test())
            .expect("the repaint must have happened");

        assert_eq!(
            light.background,
            bundled_rgb("Paper", "terminal.background")
        );
        assert_eq!(
            light.foreground,
            bundled_rgb("Paper", "terminal.foreground")
        );
        assert_ne!(
            light.background, dark.background,
            "the terminal background must genuinely change when the theme does - this is the \
             whole of GitHub issue #208"
        );
        assert_ne!(light.foreground, dark.foreground);

        let luminance =
            |(r, g, b): (u8, u8, u8)| 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        assert!(
            luminance(light.background) > luminance(light.foreground),
            "Paper is a light theme, so its terminal must be dark text on a light fill, not the \
             other way round - got fg {:?} on bg {:?}",
            light.foreground,
            light.background
        );
    }

    #[gpui::test]
    fn two_different_dark_themes_paint_two_different_terminals(cx: &mut TestAppContext) {
        let (pane, cx) = painted_pane(cx, b"hello world");

        let ember = {
            let _theme = with_bundled_theme("Ember");
            pane.update(cx, |_pane, cx| cx.notify());
            cx.run_until_parked();
            pane.read_with(cx, |pane, _| pane.last_painted_palette_for_test())
                .expect("painted")
        };
        let moss = {
            let _theme = with_bundled_theme("Moss");
            pane.update(cx, |_pane, cx| cx.notify());
            cx.run_until_parked();
            pane.read_with(cx, |pane, _| pane.last_painted_palette_for_test())
                .expect("painted")
        };

        assert_eq!(
            ember.background,
            bundled_rgb("Ember", "terminal.background")
        );
        assert_eq!(moss.background, bundled_rgb("Moss", "terminal.background"));
        assert_ne!(
            ember, moss,
            "two bundled dark themes must not paint the terminal identically"
        );
    }

    #[gpui::test]
    fn a_named_ansi_colour_is_painted_from_the_themes_own_palette(cx: &mut TestAppContext) {
        let (pane, cx) = painted_pane(cx, b"\x1b[32mOK\x1b[0m");

        let dark_green = pane.read_with(cx, |pane, _| pane.painted_rows_for_test()[0][0].fg);
        assert_eq!(dark_green, TerminalPalette::default().ansi[2]);

        let _theme = with_bundled_theme("Paper");
        pane.update(cx, |_pane, cx| cx.notify());
        cx.run_until_parked();

        let light_green = pane.read_with(cx, |pane, _| pane.painted_rows_for_test()[0][0].fg);
        assert_eq!(
            light_green,
            bundled_rgb("Paper", "terminal.ansi.2"),
            "the cell the program printed in ANSI green must resolve against the live theme's own \
             ansi.2, not a module constant"
        );
        assert_ne!(
            light_green, dark_green,
            "Paper ships the light ANSI palette, so its green is genuinely a different colour"
        );
    }

    #[gpui::test]
    fn unstyled_output_takes_the_themes_own_foreground_and_background(cx: &mut TestAppContext) {
        let (pane, cx) = painted_pane(cx, b"plain");
        let _theme = with_bundled_theme("Slate");
        pane.update(cx, |_pane, cx| cx.notify());
        cx.run_until_parked();

        let cell = pane.read_with(cx, |pane, _| pane.painted_rows_for_test()[0][0]);
        assert_eq!(cell.c, 'p');
        assert_eq!(cell.fg, bundled_rgb("Slate", "terminal.foreground"));
        assert_eq!(cell.bg, bundled_rgb("Slate", "terminal.background"));
    }
}

/// GitHub issue #331, end to end at the pane level: real scroll-wheel/PageUp/PageDown input
/// through [`TerminalPane::handle_key_down`]/[`TerminalPane::handle_scroll_wheel`], typing
/// snapping back to the live tail, and - through a real `cat`-backed pty, not the
/// [`TerminalPane::inject_bytes_for_test`] seam - real output arriving through the real poll
/// loop while scrolled back neither moving the viewport nor going unnoticed.
#[cfg(test)]
mod scrollback_pane_tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext};

    fn new_pane(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<TerminalPane>, &mut gpui::VisualTestContext) {
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", Vec::new(), std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        // Two passes, not one: the pane's real content-bounds measurement now schedules its own
        // follow-up render via `Window::defer` (see the measuring `canvas()`'s own docs) rather
        // than updating synchronously, so a single `run_until_parked` can return with that
        // deferred callback still pending. A test that then pokes `TerminalPane`'s private state
        // directly (several in this module simulate a pre-first-paint pane) must start from a
        // fully-settled pane, or that leftover callback fires later and clobbers the test's own
        // reset with this constructor's real (large, default test-window-sized) measurement.
        cx.run_until_parked();
        cx.run_until_parked();
        (pane, cx)
    }

    /// A real, unmodified navigation key with no `key_char` - matches how GPUI reports
    /// PageUp/PageDown/arrows in practice (see `keystroke_tests`' own `keystroke` helper for the
    /// printable-character counterpart).
    fn nav_key(key: &str) -> Keystroke {
        Keystroke {
            key: key.to_string(),
            key_char: None,
            modifiers: Modifiers::default(),
        }
    }

    fn key_event(keystroke: Keystroke) -> KeyDownEvent {
        KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        }
    }

    /// Pushes `count` numbered lines directly into the grid - the same
    /// [`TerminalPane::inject_bytes_for_test`] seam `clipboard_tests`/etc. already use - enough
    /// to overflow the pane's real current row count into genuine retained scrollback.
    fn push_numbered_lines(
        pane: &gpui::Entity<TerminalPane>,
        cx: &mut gpui::VisualTestContext,
        count: usize,
    ) {
        pane.update(cx, |pane, cx| {
            for i in 0..count {
                pane.inject_bytes_for_test(format!("line {i}\r\n").as_bytes(), cx);
            }
        });
    }

    /// Pumps real poll ticks until `predicate` sees the pty round-trip land, or until a generous
    /// cap is reached, then returns the joined visible text.
    fn drain_until(
        pane: &gpui::Entity<TerminalPane>,
        cx: &mut gpui::VisualTestContext,
        predicate: impl Fn(&str) -> bool,
    ) -> String {
        let mut joined = String::new();
        for _ in 0..400 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            joined = pane
                .read_with(cx, |pane, _| pane.visible_text_lines())
                .join("");
            if predicate(&joined) {
                break;
            }
        }
        joined
    }

    #[gpui::test]
    fn page_up_and_page_down_scroll_the_real_display_offset(cx: &mut TestAppContext) {
        let (pane, cx) = new_pane(cx);
        let rows = pane.read_with(cx, |pane, _| pane.grid_dimensions().1 as usize);
        push_numbered_lines(&pane, cx, rows * 3);
        assert_eq!(pane.read_with(cx, |pane, _| pane.grid.scroll_offset()), 0);

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_key_down(&key_event(nav_key("pageup")), window, cx);
        });
        let offset = pane.read_with(cx, |pane, _| pane.grid.scroll_offset());
        assert!(
            offset > 0,
            "PageUp must move the real display_offset into history"
        );

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_key_down(&key_event(nav_key("pagedown")), window, cx);
        });
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_offset()),
            0,
            "PageDown by the same one page must land exactly back at live"
        );
    }

    #[gpui::test]
    fn page_up_never_reaches_the_real_pty(cx: &mut TestAppContext) {
        let (pane, cx) = new_pane(cx);
        let rows = pane.read_with(cx, |pane, _| pane.grid_dimensions().1 as usize);
        push_numbered_lines(&pane, cx, rows * 3);

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_key_down(&key_event(nav_key("pageup")), window, cx);
        });
        let offset_right_after = pane.read_with(cx, |pane, _| pane.grid.scroll_offset());
        let lines_right_after = pane.read_with(cx, |pane, _| pane.visible_text_lines());

        // `cat` echoes back verbatim anything it receives on stdin - if PageUp had wrongly also
        // been forwarded as pty input (rather than claimed before `keystroke_to_bytes` runs at
        // all), a real echoed reply would show up in the grid, and/or the poll loop draining it
        // could perturb `display_offset`, within a handful of real poll ticks.
        for _ in 0..20 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
        }

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_offset()),
            offset_right_after,
            "display_offset must not drift after PageUp once the poll loop has had many real \
             ticks to run - it would if a stray echoed reply had reached the pty"
        );
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.visible_text_lines()),
            lines_right_after,
            "no new content may appear after PageUp with no further input - a real echoed PageUp \
             byte sequence would show up here"
        );
    }

    #[gpui::test]
    fn typing_while_scrolled_back_snaps_back_to_the_live_tail(cx: &mut TestAppContext) {
        let (pane, cx) = new_pane(cx);
        let rows = pane.read_with(cx, |pane, _| pane.grid_dimensions().1 as usize);
        push_numbered_lines(&pane, cx, rows * 3);

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_key_down(&key_event(nav_key("pageup")), window, cx);
        });
        assert!(pane.read_with(cx, |pane, _| pane.grid.is_scrolled_back()));

        let typed = Keystroke {
            key: "a".to_string(),
            key_char: Some("a".to_string()),
            modifiers: Modifiers::default(),
        };
        pane.update_in(cx, |pane, window, cx| {
            pane.handle_key_down(&key_event(typed), window, cx);
        });

        assert!(
            !pane.read_with(cx, |pane, _| pane.grid.is_scrolled_back()),
            "a real keystroke reaching the pty must jump the view back to live"
        );
    }

    fn wheel_event(delta: gpui::ScrollDelta) -> ScrollWheelEvent {
        ScrollWheelEvent {
            position: gpui::point(px(0.0), px(0.0)),
            delta,
            modifiers: Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        }
    }

    #[gpui::test]
    fn mouse_wheel_lines_scroll_by_exactly_that_many_grid_lines(cx: &mut TestAppContext) {
        let (pane, cx) = new_pane(cx);
        let rows = pane.read_with(cx, |pane, _| pane.grid_dimensions().1 as usize);
        push_numbered_lines(&pane, cx, rows * 3);

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(gpui::point(0.0, 3.0))),
                window,
                cx,
            );
        });
        assert_eq!(pane.read_with(cx, |pane, _| pane.grid.scroll_offset()), 3);

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(gpui::point(0.0, -3.0))),
                window,
                cx,
            );
        });
        assert_eq!(pane.read_with(cx, |pane, _| pane.grid.scroll_offset()), 0);
    }

    #[gpui::test]
    fn alt_screen_scroll_forwards_page_keys_to_the_real_pty(cx: &mut TestAppContext) {
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", vec!["-v".to_string()], std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();

        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(b"\x1b[?1049h", cx); // enter the alt screen
        });
        assert!(
            pane.read_with(cx, |pane, _| pane.grid.alt_scroll_forwarding_active()),
            "sanity check: the grid must report the alt screen as active before scrolling"
        );

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(gpui::point(0.0, 2.0))),
                window,
                cx,
            );
        });

        // Real `cat -v` printing back exactly what it received on stdin - pump the poll loop
        // until it lands.
        let joined = drain_until(&pane, cx, |text| text.contains("^[[5~"));

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_offset()),
            0,
            "the alt screen has no scrollback of its own - scroll_display must never have moved"
        );
        assert_eq!(
            joined.matches("^[[5~").count(),
            1,
            "a two-line upward scroll is under one wheel notch, so it must forward exactly one \
             real PageUp byte sequence, visualized by `cat -v` as the literal text `^[[5~`: \
             {joined:?}"
        );
        assert_eq!(
            joined.matches("^[[6~").count(),
            0,
            "an upward scroll must never also send a PageDown byte sequence: {joined:?}"
        );
        assert_eq!(
            joined.matches("^[[A").count() + joined.matches("^[[B").count(),
            0,
            "GitHub issue #368: no arrow-key bytes may reach the child any more - forwarding \
             those is what made Claude Code's CLI tell the user to press PgUp/PgDn instead: \
             {joined:?}"
        );
    }

    #[gpui::test]
    fn alt_screen_scroll_down_forwards_page_down_keys(cx: &mut TestAppContext) {
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", vec!["-v".to_string()], std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();

        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(b"\x1b[?1049h", cx); // enter the alt screen
        });

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(gpui::point(0.0, -3.0))),
                window,
                cx,
            );
        });

        let joined = drain_until(&pane, cx, |text| text.contains("^[[6~"));
        assert_eq!(
            joined.matches("^[[6~").count(),
            1,
            "a three-line downward scroll is exactly one wheel notch \
             ([`WHEEL_LINES_PER_NOTCH`]), so it must forward exactly one real PageDown byte \
             sequence - not one per line, which would fling a full-screen program three \
             screenfuls away on a single detent: {joined:?}"
        );
        assert_eq!(joined.matches("^[[5~").count(), 0);
        assert_eq!(
            joined.matches("^[[A").count() + joined.matches("^[[B").count(),
            0
        );
    }

    #[gpui::test]
    fn a_fast_alt_screen_flick_forwards_proportionally_more_page_keys(cx: &mut TestAppContext) {
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", vec!["-v".to_string()], std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();

        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(b"\x1b[?1049h", cx); // enter the alt screen
        });

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(gpui::point(
                    0.0,
                    3.0 * WHEEL_LINES_PER_NOTCH,
                ))),
                window,
                cx,
            );
        });

        let joined = drain_until(&pane, cx, |text| text.matches("^[[5~").count() >= 3);
        assert_eq!(
            joined.matches("^[[5~").count(),
            3,
            "three notches of upward delta must forward three real PageUp presses: {joined:?}"
        );
    }

    #[gpui::test]
    fn a_real_page_key_reaches_a_full_screen_program(cx: &mut TestAppContext) {
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            TerminalPane::new(
                TerminalSpec::command("cat", vec!["-v".to_string()], std::env::temp_dir()),
                ROW_FONT_SIZE_PX,
                cx,
            )
        });
        cx.run_until_parked();

        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(b"\x1b[?1049h", cx); // enter the alt screen
        });
        assert!(
            pane.read_with(cx, |pane, _| pane.grid.alt_screen_active()),
            "sanity check: the alt screen must really be active"
        );

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_key_down(&key_event(nav_key("pageup")), window, cx);
            pane.handle_key_down(&key_event(nav_key("pagedown")), window, cx);
        });

        let joined = drain_until(&pane, cx, |text| {
            text.contains("^[[5~") && text.contains("^[[6~")
        });
        assert_eq!(
            joined.matches("^[[5~").count(),
            1,
            "a real PageUp press must cross the pty as the standard `ESC [ 5 ~`: {joined:?}"
        );
        assert_eq!(
            joined.matches("^[[6~").count(),
            1,
            "a real PageDown press must cross the pty as the standard `ESC [ 6 ~`: {joined:?}"
        );
    }

    #[gpui::test]
    fn leaving_the_alt_screen_restores_real_scrollback_on_the_next_scroll(cx: &mut TestAppContext) {
        let (pane, cx) = new_pane(cx);
        let rows = pane.read_with(cx, |pane, _| pane.grid_dimensions().1 as usize);
        push_numbered_lines(&pane, cx, rows * 3);

        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(b"\x1b[?1049h", cx); // enter the alt screen
            pane.inject_bytes_for_test(b"\x1b[?1049l", cx); // and leave it again
        });
        assert!(
            !pane.read_with(cx, |pane, _| pane.grid.alt_scroll_forwarding_active()),
            "sanity check: back on the normal screen"
        );

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(gpui::point(0.0, 4.0))),
                window,
                cx,
            );
        });

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_offset()),
            4,
            "back on the normal screen, a scroll must move real scroll_display exactly as \
             before this issue - not still be forwarded to the child as key presses"
        );
    }

    #[gpui::test]
    fn a_fresh_pane_never_becomes_scrollable_as_its_layout_settles(cx: &mut TestAppContext) {
        let (pane, cx) = new_pane(cx);

        // A real shell's startup output: the blank line most prompts print before themselves,
        // then a long single-line prompt that genuinely has to re-wrap at the narrower widths
        // below - the exact shape measured in the running app.
        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(
                b"\r\n/tmp/fix-terminal-scroll-round3 on main! at 0:55:28\r\n$ ",
                cx,
            );
        });
        cx.run_until_parked();

        // Every size the app's own layout can settle the pane through, widest to narrowest and
        // back - a window resize, a panel opening, a font-size change all land here. All of them
        // still comfortably fit the prompt itself; a pane narrower than its own prompt really
        // does push part of that prompt out of view, and being able to scroll up to read it is
        // correct - `terminal::grid`'s own
        // `a_resize_never_leaves_a_blank_line_at_the_top_of_the_scroll_track` carries that case.
        for (width, rows) in [
            (800.0f32, 36.0f32),
            (300.0, 26.0),
            (300.0, 14.0),
            (900.0, 40.0),
            (420.0, 20.0),
        ] {
            // A *real* resize of the test window, not a direct `content_bounds` field poke:
            // `TerminalPane` is this window's root view (`.size_full()`, no ancestor chrome), so
            // resizing the window to exactly `(width, rows * ROW_LINE_HEIGHT_PX)` makes the
            // pane's own real, measured padding-box bounds exactly that size too - and, since
            // the pane's own measuring `canvas()` now actively self-heals any divergence from
            // the real measurement (see that `canvas()` call's own docs), a direct field poke
            // here would just be corrected right back by the next real paint anyway.
            cx.simulate_resize(gpui::size(px(width), px(rows * ROW_LINE_HEIGHT_PX)));
            cx.run_until_parked();
            cx.run_until_parked();

            let dims = pane.read_with(cx, |pane, _| pane.grid_dimensions());
            assert_eq!(
                pane.read_with(cx, |pane, _| pane.grid.scroll_history_len()),
                0,
                "a pane that has only ever printed a prompt must have nothing to scroll to at \
                 {dims:?}"
            );

            pane.update_in(cx, |pane, window, cx| {
                pane.handle_scroll_wheel(
                    &wheel_event(gpui::ScrollDelta::Lines(gpui::point(0.0, 5.0))),
                    window,
                    cx,
                );
            });
            assert_eq!(
                pane.read_with(cx, |pane, _| pane.grid.scroll_offset()),
                0,
                "and the wheel must be genuinely inert there - this is the exact user-visible \
                 symptom: an empty terminal that scrolls"
            );
            assert!(
                cx.debug_bounds("terminal-scrollbar").is_none(),
                "nor may the scrollbar be painted at {dims:?}"
            );
        }

        // The guard must not have cost the pane real scrollback: genuine overflow at the
        // settled size still works exactly as before.
        let rows = pane.read_with(cx, |pane, _| pane.grid_dimensions().1 as usize);
        push_numbered_lines(&pane, cx, rows * 3);
        assert!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_history_len()) > 0,
            "real overflow must still create real scrollback"
        );
        assert!(cx.debug_bounds("terminal-scrollbar").is_some());
    }

    #[gpui::test]
    fn settling_waits_for_the_resize_to_reach_the_real_pty(cx: &mut TestAppContext) {
        let (pane, cx) = new_pane(cx);

        // Roll back to the pre-spawn state `TerminalPane::new` really starts in: a
        // placeholder-sized grid, nothing measured, and - the part issue #362 missed - no live
        // session for a resize to reach.
        let session = pane.update(cx, |pane, _cx| {
            pane.grid = TerminalGrid::new(TERMINAL_ROWS, TERMINAL_COLS);
            pane.content_bounds = None;
            pane.settled_real_size = false;
            pane.resize_latch = ResizeLatch::default();
            pane.session.take()
        });

        let bounds = Bounds {
            origin: gpui::point(px(0.0), px(0.0)),
            size: gpui::size(px(800.0), px(20.0 * ROW_LINE_HEIGHT_PX)),
        };
        pane.update_in(cx, |pane, window, _cx| {
            pane.content_bounds = Some(bounds);
            pane.maybe_resize_pty(window);
        });
        assert!(
            !pane.read_with(cx, |pane, _| pane.settled_real_size),
            "a measured resize that could not reach a pty - because the async spawn has not \
             finished - must not count as settled: the child is still running at the spawn-time \
             placeholder size, which is exactly the window this guard exists to cover"
        );

        pane.update(cx, |pane, _cx| pane.session = session);
        pane.update_in(cx, |pane, window, _cx| pane.maybe_resize_pty(window));
        assert!(
            pane.read_with(cx, |pane, _| pane.settled_real_size),
            "once the resize really reached the live pty, the pane is settled and the one-time \
             discard has run"
        );
    }

    #[gpui::test]
    fn a_fresh_pane_with_little_content_shows_no_scrollbar(cx: &mut TestAppContext) {
        let (pane, cx) = new_pane(cx);
        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(b"$ echo hello\r\nhello\r\n$ ", cx);
        });
        cx.run_until_parked();

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_history_len()),
            0,
            "a handful of lines, well under the real viewport height, must never manufacture \
             scrollback"
        );
        assert!(
            cx.debug_bounds("terminal-scrollbar").is_none(),
            "the scrollbar must not be painted when there is nothing to scroll to"
        );

        let rows = pane.read_with(cx, |pane, _| pane.grid_dimensions().1 as usize);
        push_numbered_lines(&pane, cx, rows * 3);

        assert!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_history_len()) > 0,
            "sanity check: this really did overflow"
        );
        assert!(
            cx.debug_bounds("terminal-scrollbar").is_some(),
            "genuine overflow must still show the scrollbar - this fix must not suppress it"
        );
    }

    #[gpui::test]
    fn the_placeholder_spawn_races_content_bounds_but_the_first_real_resize_discards_it(
        cx: &mut TestAppContext,
    ) {
        let (pane, cx) = new_pane(cx);

        // Roll back to the pre-first-paint state `TerminalPane::new` actually starts in -
        // `new_pane`'s own `add_window_view` call already settled this test pane once, so this
        // simulates a pane whose very first corrective resize hasn't happened yet.
        pane.update(cx, |pane, _cx| {
            pane.grid = TerminalGrid::new(TERMINAL_ROWS, TERMINAL_COLS);
            pane.content_bounds = None;
            pane.settled_real_size = false;
            pane.resize_latch = ResizeLatch::default();
        });

        // The child prints output that comfortably fits the placeholder's own height but will
        // not fit the small real size below - exactly the race this issue reports.
        let placeholder_rows = TERMINAL_ROWS as usize;
        let real_rows = 10u16;
        assert!(
            placeholder_rows > real_rows as usize,
            "sanity check: the placeholder must genuinely be taller than the real target"
        );

        // The child's output lands directly on the grid, bypassing `inject_bytes_for_test`'s own
        // `cx.notify()` (unlike every other test in this module) - a real pty's bytes arrive on
        // a background poll tick this test isn't driving, so nothing here may trigger a real
        // render on its own. That distinction matters more than it used to: the pane's own
        // measuring `canvas()` now self-heals toward the real window on every real paint it sees
        // (see that `canvas()` call's own docs) - if injecting *did* trigger a render here, this
        // pane would discover this test's real (large, default) window mid-loop and settle
        // against *that*, long before this test ever gets to simulate the real race, which is
        // exactly the failure mode this distinction avoids.
        pane.update(cx, |pane, _cx| {
            for i in 0..(placeholder_rows - 5) {
                pane.grid.append_bytes(format!("line {i}\r\n").as_bytes());
            }
        });
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_history_len()),
            0,
            "sanity check: this content fits the placeholder's own 48-row height, so nothing has \
             overflowed yet"
        );

        // The pane's own content-box measurement settles to a real, much smaller area - what
        // `render`'s measuring `canvas()` reports once it has actually painted - driven directly
        // rather than through a real resize/paint cycle for the same reason the injection above
        // is: this test's real window is this pane's own default (large) test size, not the
        // small target below, so a real `cx.simulate_resize` here would just as readily paint an
        // intermediate frame at the *real* window size first (see `a_fresh_pane_never_becomes_
        // scrollable_as_its_layout_settles` for that scenario instead - a real resize sequence
        // on a pane that starts out already settled).
        let small_bounds = Bounds {
            origin: gpui::point(px(0.0), px(0.0)),
            size: gpui::size(px(800.0), px(real_rows as f32 * ROW_LINE_HEIGHT_PX)),
        };
        pane.update_in(cx, |pane, window, _cx| {
            pane.content_bounds = Some(small_bounds);
            pane.maybe_resize_pty(window);
        });

        assert!(
            pane.read_with(cx, |pane, _| pane.grid_dimensions().1) <= real_rows + 1,
            "sanity check: the corrective resize must have actually shrunk the grid"
        );
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_history_len()),
            0,
            "the very first real resize must discard whatever the placeholder-sized spawn \
             window manufactured - the human never saw this content overflow at the size \
             they're actually looking at"
        );
        assert!(pane.read_with(cx, |pane, _| pane.settled_real_size));

        // Later, genuine overflow (more output than the now-correctly-sized real viewport
        // holds - the normal, expected behavior PR #351 shipped) must still create real
        // scrollback exactly as before this fix - only the one-time placeholder-spawn race
        // above is special-cased, not overflow in general.
        pane.update(cx, |pane, cx| {
            for i in 0..50 {
                pane.inject_bytes_for_test(format!("more {i}\r\n").as_bytes(), cx);
            }
        });
        assert!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_history_len()) > 0,
            "output that genuinely overflows the real, already-settled viewport must still \
             create real scrollback - this fix must only special-case the one-time \
             placeholder-spawn race, not overflow in general"
        );

        // A second, genuine resize (an actual window resize happening after settling) must
        // also still be able to create real scrollback via the ordinary shrink path.
        let smaller_bounds = Bounds {
            origin: gpui::point(px(0.0), px(0.0)),
            size: gpui::size(px(800.0), px(3.0 * ROW_LINE_HEIGHT_PX)),
        };
        let history_before_second_resize =
            pane.read_with(cx, |pane, _| pane.grid.scroll_history_len());
        pane.update_in(cx, |pane, window, _cx| {
            pane.content_bounds = Some(smaller_bounds);
            pane.maybe_resize_pty(window);
        });
        assert!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_history_len())
                >= history_before_second_resize,
            "a second, genuine resize after settling must still be able to grow real scrollback \
             via the ordinary shrink path - this fix must not have latched some permanent \
             discard-on-every-resize behavior"
        );
    }

    #[gpui::test]
    fn sub_line_trackpad_deltas_accumulate_into_a_real_line_scroll(cx: &mut TestAppContext) {
        let (pane, cx) = new_pane(cx);
        let rows = pane.read_with(cx, |pane, _| pane.grid_dimensions().1 as usize);
        push_numbered_lines(&pane, cx, rows * 3);

        let row_height = pane.read_with(cx, |pane, _| pane.line_height_px());
        let two_thirds_row = row_height * 2.0 / 3.0;

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Pixels(gpui::point(
                    px(0.0),
                    px(two_thirds_row),
                ))),
                window,
                cx,
            );
        });
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_offset()),
            0,
            "a single sub-line delta must not itself move a whole line yet"
        );

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Pixels(gpui::point(
                    px(0.0),
                    px(two_thirds_row),
                ))),
                window,
                cx,
            );
        });
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_offset()),
            1,
            "two 2/3-row deltas together exceed one full row and must produce a real one-line \
             scroll, not be dropped individually"
        );
    }

    #[gpui::test]
    fn new_real_pty_output_while_scrolled_back_does_not_move_the_viewport_and_latches_the_indicator(
        cx: &mut TestAppContext,
    ) {
        let (pane, cx) = new_pane(cx);
        let rows = pane.read_with(cx, |pane, _| pane.grid_dimensions().1 as usize);
        let line_count = rows * 3;

        let first_batch: String = (0..line_count).map(|i| format!("line {i}\r\n")).collect();
        pane.update(cx, |pane, cx| {
            pane.session
                .as_ref()
                .expect("a real cat session must be live")
                .write_input(first_batch.as_bytes())
                .expect("writing to a real live pty must succeed");
            cx.notify();
        });
        let last_line = format!("line {}", line_count - 1);
        let mut echoed = false;
        for _ in 0..300 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            if pane.read_with(cx, |pane, _| {
                pane.visible_text_lines()
                    .iter()
                    .any(|line| line.contains(&last_line))
            }) {
                echoed = true;
                break;
            }
        }
        assert!(echoed, "the real pty must echo the first batch back");

        pane.update_in(cx, |pane, window, cx| {
            pane.handle_key_down(&key_event(nav_key("pageup")), window, cx);
        });
        let offset_before = pane.read_with(cx, |pane, _| pane.grid.scroll_offset());
        assert!(offset_before > 0, "sanity check: genuinely scrolled back");
        assert!(
            !pane.read_with(cx, |pane, _| pane.new_output_while_scrolled),
            "sanity check: no new output has arrived yet"
        );
        // The real content on screen right now - what "stay put" promises stays visible. Not
        // `scroll_offset()` itself: "staying put" means `display_offset` keeps pace with however
        // many real grid rows the new output consumes (matching `alacritty_terminal`'s own
        // `Grid::scroll_up` pinning behavior - see `TerminalGrid::scroll_display`'s docs), which
        // is exactly what makes the *visible lines* the invariant here, not the raw offset
        // number (a wrapped long line can advance the grid by more than one row per logical
        // line written).
        let lines_before = pane.read_with(cx, |pane, _| pane.visible_text_lines());

        pane.update(cx, |pane, cx| {
            pane.session
                .as_ref()
                .expect("a real cat session must still be live")
                .write_input(b"more output while scrolled back\r\n")
                .expect("writing to a real live pty must succeed");
            cx.notify();
        });
        let mut saw_new_output_flag = false;
        for _ in 0..300 {
            cx.background_executor.advance_clock(POLL_INTERVAL);
            cx.run_until_parked();
            if pane.read_with(cx, |pane, _| pane.new_output_while_scrolled) {
                saw_new_output_flag = true;
                break;
            }
        }
        assert!(
            saw_new_output_flag,
            "real output arriving through the real poll loop while scrolled back must latch \
             the jump-to-bottom affordance's indicator"
        );
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.visible_text_lines()),
            lines_before,
            "must not move the viewport off the lines the user was looking at"
        );
        assert!(
            pane.read_with(cx, |pane, _| pane.grid.scroll_offset()) >= offset_before,
            "display_offset must have kept pace with (or exceeded, if the new line wrapped) the \
             real new output, not snapped back toward live"
        );

        // The jump-to-bottom affordance's own click handler - exercised directly, since it
        // needs a real painted hitbox to click through GPUI's own event dispatch - clears both.
        pane.update(cx, |pane, cx| {
            pane.grid.scroll_display(ScrollAmount::Bottom);
            pane.new_output_while_scrolled = false;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!pane.read_with(cx, |pane, _| pane.grid.is_scrolled_back()));
        assert!(!pane.read_with(cx, |pane, _| pane.new_output_while_scrolled));
    }

    #[test]
    fn terminal_scroll_handle_reports_live_at_the_bottom_and_history_at_the_top() {
        let handle = TerminalScrollHandle::new();
        let bounds = Bounds {
            origin: gpui::point(px(0.0), px(0.0)),
            size: gpui::size(px(200.0), px(500.0)),
        };
        handle.sync(bounds, px(20.0), 100, 0);

        assert_eq!(handle.max_scroll_offset(), gpui::point(px(0.0), px(2000.0)));
        assert_eq!(
            handle.scroll_offset(),
            gpui::point(px(0.0), px(-2000.0)),
            "display_offset == 0 (live) must report the maximum negative offset - the bottom of \
             the track, matching every real terminal emulator's own scrollbar"
        );

        handle.sync(bounds, px(20.0), 100, 100);
        assert_eq!(
            handle.scroll_offset(),
            gpui::point(px(0.0), px(0.0)),
            "fully scrolled back (display_offset == history_len) must report offset zero - the \
             top of the track"
        );

        handle.sync(bounds, px(20.0), 100, 100);
        handle.set_scroll_offset(gpui::point(px(0.0), px(-1900.0)));
        assert_eq!(handle.take_requested_display_offset(), Some(5));
    }
}
