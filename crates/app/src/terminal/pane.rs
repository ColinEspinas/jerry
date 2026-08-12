//! A single terminal-backed pane: a real process (a plain shell, or an agent CLI like
//! `claude`) spawned via `pty-core`, streamed into a real `alacritty_terminal`-backed
//! [`crate::terminal::grid::TerminalGrid`] and rendered as a genuine cursor-addressed
//! terminal grid, not a scrolling plain-text log (see `crate::terminal::grid`'s module docs
//! for why that distinction matters for agent CLIs). What gets spawned is described by
//! [`TerminalSpec`] - `TerminalPane` itself has no notion of "shell" vs. "agent CLI". The
//! agent/tab bookkeeping that decides which spec to use, and that owns more than one
//! `TerminalPane` as tabs, lives in `crate::work_surface::agents`/`crate::root`, one layer up.
//!
//! ## Offloading blocking work off the GPUI foreground thread
//!
//! `pty_core::spawn` performs blocking I/O, so it runs via `cx.background_executor().spawn(..)`
//! (verified at `vendor/zed/crates/gpui/src/executor.rs:89`; its `timer(..)` sibling used
//! below is at `:162`, same `impl BackgroundExecutor`), not on the foreground/UI thread.
//! `PtySession::output()` is a bounded `std::sync::mpsc::Receiver<Vec<u8>>`; rather than a
//! second dedicated OS thread to bridge it, this drains it with non-blocking `try_recv()`
//! from inside a GPUI foreground async task that wakes every [`POLL_INTERVAL`] (or, for a
//! pane whose agent isn't the globally active tab, every [`BACKGROUND_POLL_INTERVAL`] -
//! see [`tick_cadence`]) via `cx.background_executor().timer(..)`. `try_recv()` never
//! blocks, but see [`MAX_BYTES_PER_TICK`] for why the drain loop itself is still bounded -
//! "never blocks" isn't the same as "bounded work per call".
//!
//! This is a *poller*, and that is a real, deliberate difference from Zed's own terminal, which
//! is event-driven: `vendor/zed/crates/terminal/src/terminal.rs`'s event loop blocks on
//! `self.events_rx.next().await` (a `futures` channel fed by `alacritty_terminal`'s own reader
//! thread), processes the first event immediately "for lowered latency", and only then
//! coalesces further events behind a 4ms timer. It can do that because its producer is an async
//! channel; `pty_core::PtySession::output()` is a `std::sync::mpsc::Receiver`, which has no
//! `await`, so bridging it event-driven would need a second OS thread per pane purely to turn
//! blocking `recv()`s into wakeups. Polling avoids that thread, at the cost of up to one
//! [`POLL_INTERVAL`] of latency per byte - which is why that interval is sized against the
//! frame budget rather than picked as a "redraw rate"; see its own docs.
//!
//! ## Input
//!
//! Typed keys are forwarded to the child process: [`TerminalPane`] is focusable
//! (`cx.focus_handle()` + `.track_focus(..)`, the same pattern
//! `vendor/zed/crates/terminal_view/src/terminal_view.rs` uses), and [`keystroke_to_bytes`]
//! turns a `gpui::Keystroke` into the bytes a real terminal would send (printable characters,
//! Enter/Backspace/Tab/Escape/arrows, `Ctrl`+letter control codes). This is a deliberately
//! small subset of `vendor/zed/crates/terminal/src/mappings/keys.rs`'s `to_esc_str` (which
//! needs more `alacritty_terminal` terminal-mode state than this pane threads through, e.g.
//! application cursor-key mode) - enough to type commands, use arrow keys, and send
//! `Ctrl-C`, not a full VT100 keymap.
//!
//! ## Double-width characters (GitHub issue #211)
//!
//! CJK ideographs and most emoji occupy two grid columns. `crate::terminal::grid` labels those
//! cells (`CellWidth`, from `alacritty_terminal`'s own `WIDE_CHAR`/`WIDE_CHAR_SPACER` flags); this
//! module is what acts on the label, in [`row_runs`]/[`wide_run_width`]:
//!
//! - The spacer cell the emulator writes after a wide character paints nothing at all. Painting it
//!   (which is what this module used to do, having no idea it existed) put a stray blank column
//!   after every wide glyph.
//! - A run of wide characters is pinned to an explicit `2 * cell_width` per character rather than
//!   left to whatever advance the font gives that glyph, so the column grid stays exact even when
//!   an emoji resolves through font fallback to a face with a different advance.
//!
//! Together those keep every grid column starting at exactly `column * cell_width`, which is what
//! lets [`cell_position_in_grid`]'s mouse arithmetic stay a pure function of pixels - see its own
//! docs.
//!
//! ## Windows: process-exit detection can't rely on pty EOF
//!
//! [`Self::spawn_process`]'s poll loop has exactly one unix-native trigger for "the process
//! ended": `pty_core::PtySession::output()`'s channel disconnecting (`TryRecvError::
//! Disconnected`), which fires once the reader thread inside `pty-core` observes real pty EOF.
//! On unix that happens quickly after the child exits. On Windows it structurally can't be
//! relied on at all: `pty_core`'s Windows reader thread only observes EOF once
//! `PtySession::master` itself is dropped (a `ClosePseudoConsole` chain - see that crate's
//! `run_reader_loop` docs), which killing/reaping the child does **not** do on its own. Left to
//! only the channel-disconnect path, a killed Windows process would never be noticed here:
//! [`Self::session`] would stay `Some` forever, [`Self::is_running`] would stay `true`,
//! [`Self::exit_status`] would stay `None`, and [`TerminalGrid::mark_ended`] would never run -
//! this pane's entire "the process ended" story silently breaks on that platform. So the poll
//! loop's live-session branch also independently polls `pty_core::PtySession::try_wait`
//! directly every tick under `#[cfg(windows)]`, regardless of channel state - real, cheap
//! (`GetExitCodeProcess`, non-blocking), and correct even when pty I/O itself never signals
//! anything - and feeds a confirmed exit into the exact same `newly_exited`/`process_ended`
//! state transition the unix EOF path already uses.

use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use gpui::{
    canvas, div, font, prelude::*, px, rgb, BorderStyle, Bounds, ClickEvent, Context, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyDownEvent, Keystroke, Pixels, Size, Task, Window,
};
use pty_core::{ExitStatus, PtyError, PtySession, SpawnOptions};

use crate::terminal::grid::{
    CellPosition, CellSide, CellWidth, GridCell, TerminalGrid, TerminalPalette,
};
use crate::terminal::links::{self as terminal_links, LinkMatch};
use crate::terminal::osc::Progress;
use crate::theme;

/// How often the poll task of the *globally active* agent's pane (see
/// [`TerminalPane::set_foreground`]; every other pane uses [`BACKGROUND_POLL_INTERVAL`]) wakes
/// up to drain any pty output that has arrived and, if there was any, re-render.
///
/// 8ms, i.e. roughly twice per 60Hz frame, so a byte that arrives from the child is decoded
/// into the grid within the same frame it arrived in. This was 33ms ("close to a 30fps redraw
/// rate"), which was measurably wrong on two counts, both confirmed against a real streaming
/// child in a release build:
///
/// - **It was not a redraw *rate*, it was a sleep *between* drains.** The loop's real period is
///   `POLL_INTERVAL` plus however long the tick's own work took, so 33ms produced ~24 ticks/s,
///   not 30 - and every one of those ticks found output already waiting, meaning the loop spent
///   >99% of its time asleep on top of bytes that had already arrived.
/// - **It throttled the child.** `pty-core`'s output channel is bounded and deliberately
///   backpressures (see that crate's docs), so a drain that runs 24x/s with a bounded per-tick
///   budget caps how fast the child is *allowed* to write. Measured against a child that
///   streams 4.3MB/s standalone, the pane consumed 0.80MB/s - the UI was holding the process
///   to ~19% of its natural speed, and the visible grid ran correspondingly behind.
///
/// Polling this often is not expensive when there is nothing to do: an empty tick is one
/// `try_recv` returning `Empty`, and `cx.notify()` is only called when bytes actually arrived,
/// so an idle pane still invalidates zero frames. `vendor/zed/crates/terminal/src/terminal.rs`
/// uses a 4ms window for the same job (it coalesces pty events for 4ms before re-rendering).
const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// [`POLL_INTERVAL`]'s counterpart for a pane whose agent is *not* the globally active tab
/// (see [`TerminalPane::set_foreground`]) - nobody can see a background pane's output live, so
/// polling it twice per frame buys nothing. 33ms (the pre-tightening interval, ~30 drains/s)
/// keeps a background agent's status/activity signal fresh to within a frame or two while
/// capping how much foreground-thread work each background pane can generate.
///
/// This split exists because the 8ms/[`MAX_BYTES_PER_TICK`] tightening, correct for the *one*
/// visible pane, was measured to be a real multi-pane regression: it raised every pane's
/// drainable throughput ~5x, and this app's whole point is running many agents at once. With
/// 25 concurrently-firehosing panes (release build, real X11), all-panes-at-8ms measured
/// 10-12fps with 60-70ms invalidation-to-paint latency and the foreground thread at ~80%
/// saturation, vs 17-19fps / ~27ms before the tightening - the aggregate decode work, not the
/// wake frequency, is what scales with pane count. Keying cadence on real tab visibility keeps
/// the measured single-agent throughput fix without paying it once per open agent.
const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_millis(33);

/// Defensive cap on how many output *bytes* a single poll tick will drain and decode on the
/// GPUI foreground thread. Without a cap, a firehose child (`yes`, a chatty build tool) could
/// hand the poll loop the full contents of `pty-core`'s bounded output channel (~1MB) to
/// decode in a single tick, on the same thread responsible for input handling and
/// re-rendering. Capping the per-tick budget spreads that cost across ticks instead - whatever
/// isn't drained is still sitting in the channel (pty-core's reader thread backpressures) and
/// gets picked up next tick.
///
/// This is deliberately a byte bound, not the chunk-count bound it replaced
/// (`MAX_CHUNKS_PER_TICK = 64`). A chunk is "whatever one `read(2)` on the pty master returned", which ranges
/// from a couple of dozen bytes to a full `pty_core` read buffer - so a chunk-count cap
/// bounded a tick's real work only to within orders of magnitude, and in practice bound it far
/// too tightly: measured against a real streaming child, *every single tick* hit the 64-chunk
/// cap while carrying only ~29KB, i.e. the cap was throttling throughput rather than defending
/// against a pathological tick. 256KiB is the same worst-case byte budget the old cap allowed
/// (64 chunks x pty-core's 4KiB read buffer), now expressed in the unit that actually bounds
/// the work; at the ~90MB/s `TerminalGrid::append_bytes` decodes at (measured in situ) it costs
/// the foreground thread ~3ms in the worst case. `alacritty_terminal`'s own reader bounds its
/// equivalent batch by bytes too (`MAX_LOCKED_READ = u16::MAX`, the number of bytes it will
/// process before releasing the terminal lock), for the same reason.
///
/// Note this is a *budget*, not a buffer: the drain stops once it has taken **at least** this
/// many bytes, so one oversized chunk is always taken whole. Chunks are never split, so byte
/// order and chunk boundaries reaching `TerminalGrid::append_bytes` are unchanged.
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
///
/// `eof_pending` forces the foreground cadence regardless of visibility: [`MAX_EOF_POLL_TICKS`]
/// is derived from [`POLL_INTERVAL`], so ticking the EOF grace countdown at any other interval
/// would silently stretch the real ~10s exit-confirmation window (~41s at
/// [`BACKGROUND_POLL_INTERVAL`]) - and an EOF-pending pane has nothing left to drain anyway, so
/// there is no throughput cost to bound.
///
/// A pure function (not a method) so the cadence policy is unit-testable without a GPUI
/// window - see this module's tests.
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
///
/// Derived from [`POLL_INTERVAL`] rather than written as a literal tick count, so the real
/// grace period stays ~10s of wall time no matter what the poll interval is set to. It was a
/// bare `300` back when [`POLL_INTERVAL`] happened to be 33ms; shortening the interval to 8ms
/// would silently have cut the grace to 2.4s, making the very race `eof_poll_decision` exists
/// to survive (a child that closes its pty stdio well before it exits) reappear for any child
/// that takes longer than that to exit - reported, wrongly, as
/// [`crate::rail::status::Status::Fail`].
const MAX_EOF_POLL_TICKS: u32 = (10_000 / POLL_INTERVAL.as_millis()) as u32;

/// Decides what a poll tick should do once pty EOF has been observed (the output channel's
/// `TryRecvError::Disconnected`) but the child's exit status hasn't been confirmed yet, given
/// this tick's own non-blocking `PtySession::try_wait` result.
///
/// Factored out of `TerminalPane::spawn_process`'s poll loop as a pure function so a real,
/// checker-reproduced bug is directly unit-testable without a real GPUI window or wall-clock
/// timing: the original code called `try_wait` exactly once at the moment EOF was observed,
/// and if that returned `Ok(None)` (child not yet reaped, but still alive) it gave up
/// immediately and dropped the `PtySession` - which, per `pty-core`'s `Drop` impl, signals
/// the still-live process, killing a legitimate running child out from under itself - leaving
/// `exit_status` `None` forever, which `crate::rail::status::derive_status` reads as `Status::Idle`
/// rather than `Status::Fail`. This is a reproducible race: any child that closes its
/// pty-attached stdio before actually exiting (`sh -c 'exec 0<&- 1>&- 2>&-; sleep 2; exit 7'`
/// is a minimal repro - see the end-to-end test below) triggers EOF well before it's reapable.
///
/// Returns `None` when the caller should keep the session alive and retry next tick - this
/// must *not* cause the caller to drop the `PtySession`. Returns `Some(status)` once the
/// caller should finalize: either the confirmed [`ExitStatus`], or - once
/// [`MAX_EOF_POLL_TICKS`] is exhausted - a synthetic failed status (`ExitStatus::with_signal`,
/// whose `success()` is always `false`), so a process whose exit could never be confirmed is
/// reported as [`crate::rail::status::Status::Fail`] rather than silently reverting to
/// [`crate::rail::status::Status::Idle`].
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
///
/// `terminal_font_size` is a real, persisted, user-editable setting
/// (`crate::settings::store::AppearanceSettings::terminal_font_size`, default `12.5`), so every
/// production [`TerminalPane`] is constructed with an explicit font size from that setting
/// (see [`TerminalPane::new`]) rather than this constant - this stays only as the ratio
/// [`TerminalPane::line_height_px`] scales from, and as the fixture value this module's
/// resize tests use.
const ROW_FONT_SIZE_PX: f32 = 12.0;
const ROW_LINE_HEIGHT_PX: f32 = 19.0;

/// Fallback monospace cell width, in pixels, used only until [`TerminalPane::cell_size`]'s
/// real measurement (`Window::text_system().advance`) succeeds at least once - i.e. before the
/// very first paint.
const APPROX_CELL_WIDTH_PX: f32 = 7.0;

/// The pane root's own padding, applied on every side via `.p(px(PANE_PADDING_PX))` in
/// `render`. Named as its own constant rather than left as the equivalent `.p_2()` shorthand
/// so [`TerminalPane::maybe_resize_pty`] can subtract the exact same padding it applies from
/// its own measured content-area bounds before converting to a grid size - see that method's
/// docs for the padding-box-vs-content-box bug this fixes.
///
/// `pub(crate)`, not private: `crate::code_surface`'s terminal-link click interaction
/// test needs this exact value to compute a click position off the pane's own measured
/// `content_bounds`, rather than a second, hand-copied `8.0` literal that could drift from it.
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
    ///
    /// Empty for every spawn except a Claude agent, where it carries the hook listener's port,
    /// this launch's token and the pane's own agent id (GitHub issue #239 phase 2 -
    /// `crate::hooks::HookRuntime::spawn_extras`). Those three are what let the dumb forwarder
    /// script know where to post and which row an event belongs to, and putting them in the
    /// environment rather than in the generated settings file is what allows one settings file
    /// to serve every agent this launch spawns.
    pub env: Vec<(String, String)>,
}

impl TerminalSpec {
    /// The user's shell, no extra arguments - GitHub issue #50 (real, `/bin/bash` unconditional
    /// even on Windows): `$SHELL` is a real Unix environment-variable convention that Windows
    /// itself never sets, so the fallback needs its own real Windows equivalent rather than the
    /// same Unix path unconditionally, which is a real path (`/bin/bash`) that doesn't exist on
    /// Windows at all - every default-shell spawn there was trying, and failing, to launch it.
    ///
    /// `configured` is the user's own chosen shell (GitHub issue #213 -
    /// `crate::settings::store::TerminalSettings::shell_override`, threaded here from live
    /// settings by `crate::work_surface::agents::Agents::spawn`, exactly like the terminal font
    /// size already is). `None` - the zero-config default - keeps the real OS behaviour above,
    /// byte for byte. A configured value is handed straight through as the program to spawn: a
    /// bare name (`"fish"`) is resolved against `PATH` by `pty_core::spawn`'s own
    /// `CommandBuilder`, the same already-relied-on mechanism [`Self::command`] documents, and an
    /// absolute path is used as-is. Nothing here checks whether it exists - see
    /// [`configured_shell_program`]'s docs for why that is deliberate.
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
///
/// Whitespace-only counts as "unset": the settings row backing this is a free-text field, and an
/// accidental space must fall back to the OS default rather than becoming a program named `" "`
/// that could never spawn. (`crate::settings::store::TerminalSettings::sanitize` already
/// normalizes this on load; doing it here too means a value reaching this function by any other
/// route can't reintroduce the case.)
///
/// Deliberately does **not** check that the program exists. `pty_core::spawn` is the only thing
/// that can answer that question authoritatively - it is the code that actually resolves and
/// execs it - and it already reports a real, typed `PtyError::Spawn` for a program that isn't
/// there, which `TerminalPane` surfaces as a real `spawn_error` on the failed tab
/// (`pty_core`'s own `spawn_reports_typed_error_for_nonexistent_program`). Second-guessing that
/// here could only produce a *different* answer than the real spawn (a `PATH` search of this
/// app's own is not the one `CommandBuilder` runs), which would mean silently refusing to launch
/// a shell that would in fact have worked. The Settings row shows a live, advisory
/// found/not-found hint instead (`crate::settings::state::detect_shell_status`), which informs
/// without overriding.
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

    /// The real regression: the Windows fallback must be `cmd.exe`, a real path that exists on
    /// Windows - never the Unix `/bin/bash` this crate used to fall back to unconditionally,
    /// which does not exist there at all.
    /// GitHub issue #213: a configured shell wins over whatever the OS environment says, in both
    /// of the two documented forms - a bare name (`pty_core::spawn` resolves it on `PATH`) and an
    /// absolute path (used verbatim).
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

    /// The zero-config path this setting must never disturb: no override (or a blank one, which
    /// the settings field can genuinely produce) spawns *exactly* what this app spawned before
    /// the setting existed.
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

    /// The end-to-end half of the two tests above: the exact `program` a configured bare name
    /// produces really starts a live process through the same `pty_core::spawn` a shell tab uses,
    /// and a typo'd one really fails with the typed error `TerminalPane` turns into a visible
    /// `spawn_error` on the tab - no silent blank pane either way.
    ///
    /// unix-only, like `pty-core`'s own suite: it spawns `sh`, a real binary this project's CI
    /// only runs tests on (see that crate's "Platform scope" docs).
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

    /// Trimming happens at the edge, so `" fish "` (a real, plausible copy-paste) spawns `fish`
    /// rather than a name with spaces in it that no `PATH` entry could ever match.
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
    ///
    /// This is the latch that turns the *event* the pty carries into the *state* the rail needs.
    /// [`crate::terminal::osc::OscWatcher::take_attention_ping`] is deliberately consume-on-read,
    /// because a notification is a point-in-time "now" that nothing in the protocol ever
    /// un-fires; but `crate::rail::status` is read from the render path many times a second, so
    /// consuming it *there* would make an agent's "needs input" status blink out one frame after
    /// it appeared. So the poll loop consumes the one-shot flag exactly once, here, and holds it
    /// until something real answers it: the human typing into this pane
    /// ([`Self::handle_key_down`]), or a new process starting ([`Self::spawn_process`]). It
    /// cannot leak across either boundary, and it never outlives the session that raised it.
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
    ///
    /// Deliberately "globally active", not "actually visible on screen right now": a file tab
    /// (`AdeApp::open_change`) or Settings can occupy the centre pane while the active
    /// agent's pane keeps its foreground cadence. That costs at most *one* full-cadence
    /// pane - the multi-pane aggregate this flag exists to bound is unaffected - and keeps
    /// the flag derivable from `Agents::active` alone, rather than also tracking
    /// `open_change`/`settings_open` transitions here (more writers, more ways to go stale -
    /// this codebase's recurring bug class).
    ///
    /// Maintained solely by `crate::work_surface::agents::Agents::sync_pane_cadence` (the single
    /// writer, resynced on every active-agent change); `true` at construction because
    /// `Agents::spawn` makes a new agent active in the same step.
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
    ///
    /// `#[cfg(test)]` because production has no use for it - the palette is resolved fresh every
    /// frame and consumed within that frame (see [`theme_terminal_palette`]).
    #[cfg(test)]
    last_painted_palette: Option<TerminalPalette>,
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
        };
        this.spawn_process(cx);
        this
    }

    /// Applies a Settings › Appearance "Terminal font size" edit
    /// (`crate::root::AdeApp`'s `adjust_terminal_font_size`, via
    /// `crate::work_surface::agents::Agents::set_terminal_font_size`) to this already-live pane - a
    /// no-op if the sanitized value is unchanged (e.g. a stepper click already at a clamp
    /// boundary).
    ///
    /// Invalidates [`Self::cell_width_px`] and `cx.notify()`s - the next render's
    /// [`Self::maybe_resize_pty`] call (runs unconditionally at the top of `render`) then does
    /// the real work: [`Self::cell_size`] remeasures at the new size, [`size_to_grid`]
    /// recomputes `(rows, cols)`, and [`Self::resize_to`] applies that to [`Self::grid`] and,
    /// if a session is live, a `PtySession::resize` call - the same resize path an actual
    /// window resize already drives.
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
    ///
    /// Real agent CLIs put a live status glyph here - Claude Code alternates a `\u{25d0}`-family
    /// spinner while working and rests on `\u{2733}`; Gemini CLI writes `\u{25c7}  Ready (...)`
    /// when idle (both observed directly against real CLIs; see
    /// `crate::rail::title_signal`, which is what turns this raw string into a status signal).
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
    /// would for an actual `Ctrl-C` keystroke - backs the surface footer's `Interrupt
    /// \u{2303}C` action. A no-op when no session is live.
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
    ///
    /// ## Also signals the child process, when one is live
    ///
    /// Clearing only this pane's local grid is correct for a plain shell at a prompt (it
    /// redraws on the next Enter regardless), but this terminal also runs full-screen,
    /// cursor-addressed programs (`vim`, `htop`, an agent CLI's own UI) - for those, blanking
    /// the grid alone leaves a dead screen, since the child process has no idea its output was
    /// discarded. A `Ctrl-L` (`0x0c`) written to the pty - the same [`PtySession::write_input`]
    /// path [`Self::interrupt`] uses for `Ctrl-C` - is the standard way to ask a running
    /// program to redraw. A no-op (beyond the local grid clear) when no session is live.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.grid.clear();
        if let Some(session) = &self.session {
            if let Err(err) = session.write_input(&[0x0c]) {
                log::warn!("failed to send clear-redraw signal (Ctrl-L) to pty: {err}");
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------ clipboard (GitHub issue #158)

    /// Writes this pane's real current selection to the real OS clipboard
    /// (`gpui::App::write_to_clipboard`, the same call `crate::sidebar::tree_ops::AdeApp::
    /// copy_path_to_system_clipboard` already uses for "Copy Path" - one clipboard mechanism in
    /// this app, not two). Returns whether anything was actually copied.
    ///
    /// A no-op when nothing is selected, deliberately: `Ctrl+Shift+C` with no selection must
    /// leave whatever the user copied earlier alone rather than replacing it with an empty
    /// string. See [`crate::terminal::grid::TerminalGrid::selected_text`] for what "nothing
    /// selected" covers (no anchor, an undragged anchor, or an all-blank span).
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
    ///
    /// Framing is [`paste_payload`]'s job; see its docs for why a raw write of the clipboard
    /// text would be wrong.
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

    /// Maps a window-space pointer position onto the grid cell under it, or `None` before this
    /// pane has ever painted (no measured [`Self::content_bounds`] to resolve against yet).
    ///
    /// The grid's own origin is the pane's padding box inset by [`PANE_PADDING_PX`] (see
    /// [`Self::maybe_resize_pty`]'s docs for why `content_bounds` is the padding box), pushed
    /// down one further line when the spawn-error message is occupying the first rendered line:
    /// `render` draws that message *above* the grid rows, so ignoring it would shift every
    /// computed row by one on exactly the panes that show it.
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
    ///
    /// Cached in [`Self::cell_width_px`] after the first successful measurement, falling back
    /// to [`APPROX_CELL_WIDTH_PX`] only on an outright measurement failure.
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
    ///
    /// [`Self::content_bounds`] reflects the *previous* frame's measured bounds (the measuring
    /// `canvas()` in `render` only fires during paint, after `render` itself returns) - one
    /// frame stale on the very first resize, self-correcting on the next render (the same
    /// one-frame lag `vendor/zed/crates/workspace/src/workspace.rs`'s own bounds-measurement
    /// canvas pattern has). Before any paint has happened, falls back to
    /// `window.viewport_size()` - too wide, but better than not resizing at all.
    ///
    /// [`Self::content_bounds`] itself is the *padding box*, not the content box glyphs render
    /// into: `render`'s measuring `canvas()` is `.absolute().size_full()` inside the pane root,
    /// which also carries [`PANE_PADDING_PX`] of padding - an absolutely positioned, size-full
    /// child sizes against its ancestor's padding box, while the row `div`s (normal flow) are
    /// laid out inside the *content* box, inset by that padding. Measured proof: an 844x713px
    /// pane at a 7.2x19.0px cell size computed 117 cols/37 rows from the raw padding-box
    /// measurement, but only ~115 cols/~36 rows actually fit once [`PANE_PADDING_PX`] (applied
    /// per side) is subtracted - the extra column/row painted through `overflow_hidden()`.
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
///
/// Two bugs lived here across two phases, both in *what this function was called with*, never
/// in the arithmetic itself:
///
/// - **Phase A: the wrong size.** `TerminalPane::maybe_resize_pty` used to pass the whole
///   window's `viewport_size()` unconditionally. The three-zone shell's rail/panel/chrome
///   don't belong to this pane's own content, so at a 1440px window that computed roughly 205
///   columns even though the real terminal pane is only ~820-840px wide. Fixed by passing the
///   pane's own measured content-area size (`TerminalPane::content_bounds`) instead.
/// - **Phase C: the wrong cell size.** `cell_size` was a guessed `APPROX_CELL_WIDTH_PX`/`16.0`
///   height, neither measured against the real bundled font or this pane's real rendered line
///   height. Rows had no explicit `.line_height()` before this phase, so GPUI's own default
///   (`phi()`, the golden ratio, ~19.4px at 12px font) was what actually rendered - ~21% more
///   than the `16.0` guess, so `maybe_resize_pty` always requested more rows than fit, and the
///   bottom rows rendered past the pane's `overflow_hidden()` clip. Fixed by making the line
///   height an explicit fact ([`ROW_LINE_HEIGHT_PX`], set by [`render_row`]) and measuring the
///   cell width via GPUI's font-metrics API instead of guessing - see
///   [`TerminalPane::cell_size`]'s docs.
fn size_to_grid(size: Size<Pixels>, cell_size: Size<Pixels>) -> (u16, u16) {
    let cols = ((size.width.as_f32() / cell_size.width.as_f32()) as u16).max(20);
    let rows = ((size.height.as_f32() / cell_size.height.as_f32()) as u16).max(10);
    (rows, cols)
}

/// Maps a window-space pointer position onto the grid cell under it, given the grid's own
/// painted `origin` (top-left of row 0, column 0), the measured `cell_size`, and the grid's
/// current `rows`/`cols`. Pure - independent of `Window` and this pane's state - so the
/// pixel-to-cell arithmetic is directly unit-testable, the same split [`size_to_grid`] uses.
///
/// Clamped into the grid on every side rather than returning `None` off-grid: a drag that runs
/// past the last column or below the last row is an ordinary "select to the end" gesture, and
/// every terminal treats it that way. The clamped-past-the-right-edge case resolves to
/// [`CellSide::Right`] specifically, so the final column is genuinely *included* in the
/// selection - `Selection::to_range`'s own `range_simple` drops the last cell when the
/// selection ends on a cell's left side (`selection.rs:333`).
///
/// ## Why one fixed `cell_width` is still correct with double-width characters (issue #211)
///
/// A row full of CJK looks like it should break `pixel_x / cell_width`, and it would under a
/// renderer that painted a wide glyph across two columns *and* still painted the spacer cell after
/// it. This one doesn't: [`row_runs`] drops spacer cells entirely and [`wide_run_width`] pins a
/// wide run to exactly `2 * cell_width` per character, so every grid column - wide, spacer, or
/// narrow - still starts at exactly `column * cell_width`. That invariant is the deliberate reason
/// `CellWidth::Spacer::columns()` is zero rather than one, it is asserted directly by
/// `wide_char_render_tests::painted_columns_always_sum_to_the_grid_column_count`, and it is
/// checked at real painted pixel coordinates end to end by `mouse_selection_tests::
/// a_drag_after_two_cjk_characters_still_hits_the_columns_it_looks_like_it_hits`. So this stays a
/// pure function of pixels and cell size, with no need to know the row's contents.
///
/// Clicking the *right* half of a wide character resolves to its spacer column, which is what a
/// real terminal does too - `alacritty_terminal`'s own selection handles that case
/// (`Selection::contains_cell` includes a wide char whose trailing spacer is selected,
/// `selection.rs:86`, and `Term::line_to_string` extends a selection that starts on a spacer back
/// onto the character, `term/mod.rs:584`).
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
///
/// - **Bracketed paste on** (`DECSET 2004`, which shells like `zsh`/`fish` and every full-screen
///   editor turn on): wrap in `ESC[200~`/`ESC[201~` so the program knows this was pasted, not
///   typed - that is exactly what stops a multi-line paste from auto-executing each line in a
///   shell, and what lets `vim` skip auto-indent. Any `ESC` inside the pasted text is stripped,
///   since a payload containing `ESC[201~` could otherwise close the bracket early and have the
///   rest interpreted as commands.
/// - **Bracketed paste off**: newlines are normalized to `\r`, the byte the Enter key actually
///   sends ([`keystroke_to_bytes`]'s own `"enter" => b"\r"`). A raw `\n` is a line *feed*, not a
///   carriage return, and a shell reading it in canonical mode would not run the line.
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
///
/// This split exists because of an empirically-confirmed bug: a single latched "last size"
/// field, set unconditionally on every call (including before any `PtySession` existed -
/// `TerminalPane::new` returns with `session: None`, since spawning is async), meant the very
/// first render latched a target size while there was no session to resize yet, and every
/// later call with that same computed size then short-circuited on "already up to date" -
/// permanently, even once a session appeared. The child pty stayed stuck at its spawn-time
/// size (`TERMINAL_ROWS`/`TERMINAL_COLS`) forever, diverging from what `TerminalGrid` (and the
/// rendered UI) believed the size was - confirmed via `stty -F <pty> size` against a live
/// session. A full-screen, cursor-addressed program (`vim`, `less`, an agent CLI's own UI)
/// would then paint for the wrong dimensions.
///
/// [`Self::grid`] is latched unconditionally (resizing `TerminalGrid` can't fail), but
/// [`Self::session`] is latched only by [`Self::session_resize_succeeded`] - called by
/// `TerminalPane::resize_to` only once `PtySession::resize` has returned `Ok`. A target size
/// computed before a session exists therefore never gets latched as "session in sync", so
/// `TerminalPane::spawn_process`'s success callback re-running `resize_to` at that same cached
/// target genuinely reaches the pty instead of being skipped.
/// A `(rows, cols)` pair - named so [`TerminalPane::resize_sync_state_for_test`]'s return type
/// doesn't trip clippy's `type_complexity` lint on a bare nested tuple-of-tuples.
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

/// One [`theme::ColorToken`], resolved against whichever theme is really live right now and
/// reduced to the 8-bit-per-channel RGB triple [`TerminalPalette`] carries.
///
/// 8-bit is the terminal's real precision on both ends - `alacritty_terminal` hands back `u8`
/// channels for a program-specified colour, and a theme file stores `#rrggbb` - so nothing is lost
/// here that the grid could have represented anyway.
fn token_rgb(token: theme::ColorToken) -> (u8, u8, u8) {
    let color = token.resolve();
    let channel = |component: f32| (component.clamp(0.0, 1.0) * 255.0).round() as u8;
    (channel(color.r), channel(color.g), channel(color.b))
}

/// The live theme's real terminal palette (GitHub issue #208) - `crate::theme::terminal`'s twenty
/// registered tokens resolved against whichever theme is installed right now.
///
/// This is the one place the terminal's colours cross from the theme system into
/// `crate::terminal::grid`, which is deliberately theme-free (see that module's own docs). Called
/// fresh inside [`Render::render`] rather than cached on the pane: `crate::theme`'s resolution is a
/// single hash lookup per token, and `AdeApp::apply_theme_selection` already forces a repaint of
/// every window when the selection changes (`App::refresh_windows`), so reading it at paint time
/// means a theme switch lands on the very next frame with no invalidation hook of its own to
/// forget.
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
///
/// GPUI's `BorderStyle` enum has exactly two variants, `Solid`/`Dashed`
/// (`vendor/zed/crates/gpui/src/scene.rs:597`) - no `Dotted`. `.border_dashed()` is the
/// closest honest match to the design's dotted underline; the hover state switches it back to
/// `Solid` via `Styled::style()` directly, since `Styled` exposes no `.border_solid()`
/// counterpart.
///
/// The click gesture requires holding the platform `secondary` modifier (`⌘` on macOS, `Ctrl`
/// elsewhere - the modifier the pane header's `mod + click a path to open it` hint
/// advertises) rather than firing on a bare click, so an ordinary click inside the terminal
/// (still needed for input focus) is never hijacked into a navigation; a bare click on a link
/// span still bubbles up to the pane's own root `on_click` (focus), unchanged.
///
/// Checks [`click_included_secondary_modifier`] rather than the simpler
/// `event.modifiers().secondary()` - see that function's docs for the bug that distinction
/// fixes.
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
///
/// `ClickEvent::modifiers()` (`vendor/zed/crates/gpui/src/interactive.rs:296-306`) only
/// reports the modifiers held at mouse-*up* - documented GPUI behavior, but wrong for this
/// call site: holding Ctrl, clicking, and releasing Ctrl a moment before releasing the mouse
/// button is an ordinary way to click-and-modify, and checking mouse-up alone silently drops
/// it. [`ClickEvent::Mouse`] carries both `MouseDownEvent` and `MouseUpEvent`
/// (`vendor/zed/crates/gpui/src/interactive.rs:211-217`), each with their own `modifiers`
/// field, so this checks both. `Keyboard`/`Touch` click variants have no modifiers at all, so
/// this defers to `ClickEvent::modifiers()` for them.
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
///
/// Spacer cells are dropped up front (GitHub issue #211), which does two things at once:
///
/// - They contribute no glyph, so a double-width character is no longer followed by a stray blank
///   column pushing the rest of the row right.
/// - Everything downstream sees the row's *real* text. In particular `find_links` runs against
///   `日本/x.rs` rather than `日 本 /x.rs`, and [`split_segments`]'s char offsets keep indexing
///   one painted cell each - the contract `crate::terminal::links::LinkMatch` is built on.
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
///
/// A double-width character owns exactly two grid columns, but nothing guarantees the font
/// actually advances by two monospace cells for it - a CJK ideograph in the bundled mono face
/// usually does, an emoji resolved through font fallback does not (measured: Segoe UI Emoji
/// overshoots, and before `whitespace_nowrap()` was added the overshoot made a fixed-width span
/// *wrap*, dropping the last emoji of a run onto a line of its own). Pinning each wide character
/// to `2 * cell_width` makes the row's column grid exact regardless of which face the glyph came
/// from; `flex_none()` stops the row's flex layout from shrinking the box back, and
/// `whitespace_nowrap()` stops an overshooting glyph from reflowing out of it (both applied at the
/// span-building call sites).
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
///
/// All of the row-splitting decisions live in the pure [`row_runs`]; this only turns each run
/// into an element, applying [`wide_run_width`] to the double-width ones.
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
                rgb(0xff6b6b),
                &cwd,
                cx,
            ));
        }

        for (row_index, row) in self.grid.visible_rows(&palette).into_iter().enumerate() {
            pane = pane.child(render_row(&row, row_index, &cwd, &palette, cell_width, cx));
        }

        if self.grid.ended {
            pane = pane.child(div().text_color(rgb(0xffcc66)).child("[process exited]"));
        }

        pane
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::*;
    use gpui::TestAppContext;

    /// The mutation this exists to catch - this project's recurring stale-state bug class,
    /// applied to this exact loop: hoisting the `is_foreground` read out of the poll loop
    /// (capturing it once at spawn) instead of reading it fresh every tick. All the pure
    /// [`tick_cadence`] tests below would still pass under that mutation; this one fails,
    /// because a pane spawned foreground (the only way panes spawn) would then keep draining
    /// on 8ms ticks forever, and the "nothing may drain in less than one background interval"
    /// window asserted here would see the Ctrl-L echo arrive early.
    ///
    /// Deterministic despite the real pty: the *arrival* of the echo into pty-core's channel
    /// takes real wall time (covered by a real sleep, generously sized), but the *drain* only
    /// happens on poll ticks, and those are driven purely by the test executor's fake clock.
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

    /// Empirical proof (no GPUI needed) that the race `eof_poll_decision` exists to handle is
    /// genuine: a child that closes its own pty-attached stdio before actually exiting causes
    /// the output channel to disconnect while the process is still alive - a single
    /// non-blocking `try_wait` at that moment legitimately observes nothing, and only a later
    /// retry observes the real exit status.
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

    /// Direct proof of the control-byte mapping alone: this mapping sends the standard readline
    /// "previous history" control byte (`0x10`) a focused terminal needs to receive, independent
    /// of whether GPUI's own dispatch ever actually lets a Ctrl+P keystroke reach this pane's
    /// `on_key_down` to be mapped. It no longer does in practice: `secondary-p` is now
    /// `TogglePalette`'s own real, unscoped global keybinding (see
    /// `crate::default_key_bindings`'s docs for the accepted tradeoff), so GPUI's action dispatch
    /// intercepts a focused terminal's Ctrl+P before this mapping is ever consulted. Mirrors
    /// `crate::root::focus::tab_strip_keybinding_tests::
    /// ctrl_p_opens_the_palette_even_while_a_terminal_is_focused`: that test proves the app-level
    /// dispatch *does* now intercept it; this one just pins that the pure byte-mapping function
    /// itself is unchanged, for whatever unmodified-by-a-global-binding context still reaches it.
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

    /// Direct proof of the control-byte mapping `TextUndo`/`TextRedo`'s `Some("text-input")`
    /// scoping depends on: no terminal surface ever carries `"text-input"`, specifically because
    /// `secondary-z` resolves to plain `Ctrl+Z` on Linux/Windows, and this mapping sends the real
    /// `SIGTSTP` terminal-suspend control byte (`0x1a`) a focused interactive program relies on.
    /// Mirrors `ctrl_p_maps_to_the_real_readline_previous_history_control_byte` and
    /// `crate::root::focus::text_undo_scoping_tests`'s own
    /// `secondary_z_with_a_terminal_focused_does_not_reach_text_undo`: that test proves the
    /// scoped dispatch doesn't intercept it; this one proves what reaches the pty once it
    /// doesn't.
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

    /// The CHANGELOG's contract: "a line is authored as `[prefix, colour, link, suffix]` - the
    /// link is a span inside the line, not a whole-line style". A link in the middle of an
    /// otherwise plain row must split it into a plain prefix, the link, and a plain suffix -
    /// never a whole-row link, and never dropping the surrounding plain text.
    #[test]
    fn a_link_in_the_middle_splits_into_prefix_link_suffix_not_a_whole_line_style() {
        // `"  \u{21b3} tests/upload.rs:88:"` - the link spans chars 4..19 (`tests/upload.rs:88`).
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

    /// End-to-end proof that `clear()` signals the child process: a [`TerminalPane`] backed by
    /// a real `cat` child on a real pty, [`TerminalPane::clear`] called, and what this test
    /// observes is the pty's own cooked-mode line-discipline echo - not a direct assertion
    /// that `write_input` was called, but its round-tripped effect landing back in the grid.
    /// With `ECHOCTL` (the standard-on-Linux termios default), the raw `0x0c` byte `clear()`
    /// writes is echoed back as the two printable characters `^L`.
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

    /// The side is what decides whether the cell under the pointer is included in the selection
    /// at all (`Selection::to_range`'s `range_simple` drops a cell the selection ends left of),
    /// so the half-cell boundary is real behavior, not a detail.
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

    /// Dragging back above/left of the grid origin (a real gesture - the pointer leaves the
    /// pane) must clamp to the first cell rather than underflow into a huge `usize`.
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

    /// A failed font measurement can only produce a zero cell size; dividing by it would make
    /// every index `inf as usize`. Guarded rather than left to chance.
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

    /// The real reason `ESC` is stripped: a payload carrying its own `ESC[201~` would close the
    /// bracket early and have everything after it interpreted as typed input.
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

    /// Copy with nothing selected must leave the clipboard alone. Overwriting it with `""` (the
    /// obvious way to write this) would silently destroy whatever the user copied last, every
    /// time they hit the shortcut out of habit.
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

    /// End-to-end proof that paste reaches the *child process*, observed the same way
    /// [`clear_pty_signal_tests`] proves `clear()`'s byte does: a real `cat` on a real pty
    /// echoes back what it is fed, so the pasted text appearing in the grid is round-tripped
    /// evidence the bytes genuinely left the app, not an assertion that `write_input` was
    /// called.
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

/// GitHub issue #158's mouse half, end to end through real GPUI event dispatch: before this,
/// `TerminalPane` registered no mouse-down/move/up handlers at all, so no amount of dragging
/// produced a selection and "copy" would have had nothing to copy even with a binding in place.
///
/// Backed by a real `cat` child rather than a shell: `cat` writes nothing of its own, so the
/// grid contains exactly the injected bytes and the pixel geometry the drag is computed from
/// can't be moved out from under the test by a prompt arriving mid-run.
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

    /// A plain click is how a user dismisses a selection, and it must not leave a one-character
    /// one behind.
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

    /// Moving the mouse over the terminal with no button held must never start or extend a
    /// selection - the drag has to be gated on a real mouse-down having happened.
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

    /// A drag repaints the cells it covers - the flag has to survive all the way into what
    /// `render_row` consumes, or the user gets no visual feedback at all while dragging.
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

    /// GitHub issue #211's mouse-math half, end to end through real GPUI event dispatch at real
    /// painted pixel coordinates.
    ///
    /// `cell_position_in_grid` divides by a single fixed `cell_width`, which would be wrong for
    /// any row where painted x offsets stopped matching `column * cell_width`. They don't: a wide
    /// cell paints two columns and its spacer paints none (see `CellWidth::columns`), so the
    /// offsets stay exact - and *this* is what proves it, by dragging over the `"world"` that
    /// sits four grid columns (two CJK characters) into the row and getting exactly `"world"`
    /// back. Under the pre-fix renderer those two characters painted six columns' worth of
    /// glyphs, so the same pixel positions landed two columns early.
    #[gpui::test]
    fn a_drag_after_two_cjk_characters_still_hits_the_columns_it_looks_like_it_hits(
        cx: &mut TestAppContext,
    ) {
        let (pane, cx, bounds, cell_size) = painted_pane_showing(cx, "你好world".as_bytes());

        // `你好` occupies grid columns 0..=3, so `world` is columns 4..=8.
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

    /// Dragging over the wide characters themselves - including across the second (spacer) column
    /// of the last one - must copy them once each, with no emulator-written padding spaces.
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
///
/// Reading the real path end to end turned up nothing broken to fix, and these exist to keep it
/// that way rather than to fix anything: [`keystroke_to_bytes`] hands
/// `gpui::Keystroke::key_char`'s own `String` straight to `str::as_bytes`, [`paste_payload`]'s
/// `str::replace` calls are all char-oriented, and `pty_core::PtySession::write_input` copies the
/// slice into a `Vec<u8>` that its writer thread `write_all`s. Nothing on that path is
/// byte-indexed, length-capped, or `char`-typed, so a three-byte CJK character and a
/// multi-codepoint ZWJ emoji sequence are as safe as ASCII. Regression coverage matters anyway,
/// since any of those three could grow a `.chars().next()`-shaped shortcut later.
///
/// One real limitation, stated plainly rather than papered over: this covers keys that arrive as a
/// finished `key_char`, which is what a direct (including AltGr/dead-key) keypress produces.
/// Composing text through a platform IME - the normal way CJK is *typed* - goes through GPUI's
/// `EntityInputHandler`/`Window::set_input_handler` machinery instead, which this pane does not
/// implement at all. That is a separate, larger piece of work than this issue, and it is untouched
/// here; pasting CJK works today, typing it through an IME does not.
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

    /// A three-byte character must reach the pty as all three of its bytes, in order - not as one
    /// truncated byte, and not as a replacement character.
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

    /// A grapheme cluster made of several codepoints (here a ZWJ family emoji: three emoji joined
    /// by U+200D) arrives as one `key_char` string and must be forwarded whole. Truncating to the
    /// first `char` would send a lone `👨`.
    #[test]
    fn a_multi_codepoint_grapheme_cluster_is_forwarded_whole() {
        let family = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(family.chars().count(), 5, "sanity check on the fixture");
        assert_eq!(
            keystroke_to_bytes(&typed(family)),
            Some(family.as_bytes().to_vec())
        );
    }

    /// Paste framing is `str`-level throughout, so it must not disturb multi-byte text either -
    /// in both bracketed and unbracketed mode.
    #[test]
    fn paste_framing_leaves_multi_byte_text_byte_for_byte_intact() {
        let text = "日本語 🎉";
        assert_eq!(paste_payload(text, false), text);
        assert_eq!(
            paste_payload(text, true),
            format!("\x1b[200~{text}\x1b[201~")
        );
    }

    /// The real round trip, through everything: a real `KeyDownEvent` into the pane's own
    /// `handle_key_down`, out through `PtySession::write_input`'s writer thread to a real pty, and
    /// back as a real `cat` process's echo - which the grid then parses as real UTF-8. What is
    /// asserted is the character landing in the grid as a genuine wide cell, so nothing along that
    /// path can have mangled the bytes.
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
///
/// These drive real UTF-8 through a real `TerminalGrid` (the same `alacritty_terminal` parser
/// production uses) and assert on [`row_runs`], which is genuinely what `render_row` turns into
/// elements - not a re-derivation of the layout a second way.
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

    /// The invariant the whole design rests on, and the reason `cell_position_in_grid`'s fixed
    /// `pixel_x / cell_width` arithmetic needed no change: however many wide characters a row
    /// contains, the painted columns still add up to exactly the grid's own column count, so the
    /// x offset of grid column `k` is always `k * cell_width`.
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

    /// The bug itself: the spacer cell must contribute no glyph. Painting the whole row's runs
    /// back to a string has to give the characters the program actually printed, once each.
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

    /// Every wide character is its own two-column run - never merged with the next one, so a glyph
    /// whose real advance overshoots two cells can't accumulate that overshoot across a run.
    /// Narrow text still merges into as few runs as possible.
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

    /// The pixel width a wide run is pinned to, and the fact a narrow one is left unpinned (it can
    /// use its glyphs' natural monospace advance, which is already correct).
    #[test]
    fn only_a_wide_run_gets_an_explicit_pixel_width() {
        let runs = row_runs(&row_zero("你好world", 20));
        assert_eq!(wide_run_width(&runs[0], px(8.0)), Some(px(16.0)));
        assert_eq!(wide_run_width(&runs[2], px(8.0)), None);
    }

    /// A narrow run and a wide run can never be merged, whatever their colours - the merged span
    /// would be sized for the wrong number of columns.
    #[test]
    fn a_wide_run_never_merges_into_the_narrow_text_around_it() {
        let runs = row_runs(&row_zero("a好b", 20));
        let shapes: Vec<(&str, usize)> = runs
            .iter()
            .map(|run| (run.text.trim_end(), run.columns))
            .collect();
        assert_eq!(shapes[0], ("a", 1));
        assert_eq!(shapes[1], ("好", 2));
        // The trailing run is `b` plus the row's blank remainder.
        assert_eq!(&shapes[2].0[..1], "b");
    }

    /// Link detection has to see the row's real text, not the emulator's padding. With the spacer
    /// cells still in the row text this reads `/tmp/日 本 /main.rs`, whose space breaks the path
    /// apart; the link then either fails to match or matches the wrong span.
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

    /// A link sitting *after* a wide character still gets its own span, and the plain text around
    /// it is preserved - i.e. `split_segments`'s char offsets and the painted cells stayed in
    /// lockstep once the spacers were dropped.
    #[test]
    fn a_link_after_a_wide_character_still_lands_on_the_right_characters() {
        let runs = row_runs(&row_zero("好 src/main.rs done", 40));
        let linked: Vec<&RowRun> = runs.iter().filter(|run| run.link.is_some()).collect();
        let link_text: String = linked.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(link_text, "src/main.rs");

        let painted: String = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(painted.trim_end(), "好 src/main.rs done");
    }

    /// Selection is still carried per wide character - a run that lost the flag halfway would
    /// paint the selection fill across characters that aren't selected.
    #[test]
    fn the_selection_flag_survives_onto_the_wide_characters_it_covers() {
        let mut grid = TerminalGrid::new(2, 20);
        grid.append_bytes("你好世界".as_bytes());
        // Columns 0..=3 are `你好` (each a wide cell plus its spacer).
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

    /// `visible_text_lines` backs the rail's question preview, which is plain text a human reads -
    /// it must not carry the emulator's padding spaces either.
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
///
/// Before this, `crate::terminal::grid` held them as module constants (VS Code's own default
/// terminal palette), so every one of this app's six themes rendered the terminal identically - a
/// `#1e1e1e` rectangle sitting inside a `#0d0f11` app, unchanged by switching to the light theme.
///
/// These assert on [`TerminalPane::last_painted_palette_for_test`], which `Render::render` itself
/// writes - so what is checked is the palette a real paint genuinely used, not one this test
/// re-derived a second way.
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

    /// `crate::terminal::grid` stays theme-free, so it carries its own transcription of Jerry
    /// Dark's terminal palette. This is what stops that copy from going stale: with no theme
    /// installed (the real Jerry Dark identity case), resolving the real tokens must produce
    /// exactly it.
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

    /// The headline behaviour. The *same* pane, painted twice, must genuinely change colour when
    /// the theme changes - and land on the light theme's own real values, not merely on something
    /// different.
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

    /// Two visually distinct *dark* themes must differ too - a guard against a fix that only
    /// happened to work because one bundled theme inverts lightness.
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

    /// A colour the *running program* asked for by name (`SGR 32`, "green") has to come out of the
    /// theme's own palette too, not only the pane's default fill and text - that is the half a fix
    /// that only retinted the background would miss entirely.
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

    /// Unstyled text - the overwhelming majority of what a terminal paints - must take the theme's
    /// foreground, and an unstyled cell's background must be the theme's terminal background.
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
