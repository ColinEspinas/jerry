//! The OSC 9 / OSC 9;4 / OSC 777 side-channel: desktop notifications and progress reports that
//! real agent CLIs already emit into their pty and that `alacritty_terminal` throws away.
//!
//! ## Why this is a second, independent parser
//!
//! The primary rendering pipeline (`crate::terminal::grid`) drives
//! `alacritty_terminal::vte::ansi::Processor` over a `Term`, and `Processor`'s own
//! `vte::Perform::osc_dispatch` impl (`vte-0.15.0/src/ansi.rs:1329`, read from the vendored
//! source this workspace actually compiles) has match arms for exactly OSC
//! `0`/`2`/`4`/`8`/`10`/`11`/`12`/`22`/`50`/`52`/`104`/`110`/`111`/`112`. Everything else -
//! including the whole OSC 9 family and OSC 777 - falls into its catch-all `_ => unhandled(params)`
//! arm, which formats the params into a `debug!` log line and drops them. There is no
//! `alacritty_terminal::event::Event` variant for them either, so no amount of widening
//! `crate::terminal::grid`'s `EventListener` can recover them: the data is gone before any
//! listener could see it.
//!
//! So this module tees: [`OscWatcher::feed`] is handed the *same* byte slice
//! `crate::terminal::grid::TerminalGrid::append_bytes` hands `Processor::advance`, and runs its
//! own [`vte::Parser`] over it with a [`vte::Perform`] impl that implements *only*
//! `osc_dispatch` (every other `Perform` method keeps its no-op default, so printing, CSI
//! dispatch, DCS, etc. cost nothing here beyond the state machine's own transitions).
//!
//! Teeing rather than extending is deliberate and load-bearing: the primary pipeline has a
//! documented history of subtle real bugs (see `crate::terminal::grid`'s `TermEventSink` docs
//! for the ConPTY handshake hang), so it is treated as untouchable - a bug in this module's OSC
//! handling can make Jerry's rail guess wrong, but it can never corrupt what the user sees on
//! screen.
//!
//! ## What is parsed
//!
//! - **OSC 9** (`ESC ] 9 ; <message> BEL`) - iTerm2's "post a desktop notification", also emitted
//!   by Claude Code and Gemini CLI. A point-in-time "the human's attention is wanted" event; the
//!   message text is deliberately *not* kept (see the parent issue: coarse status only, no
//!   free-text activity in this phase).
//! - **OSC 9;4** (`ESC ] 9 ; 4 ; <state> ; <progress> BEL`) - ConEmu's taskbar-progress protocol,
//!   also spoken by Windows Terminal, WezTerm and others. `state` is `0` clear / `1` normal /
//!   `2` error / `3` indeterminate / `4` paused; `progress` is a percentage `0..=100`, and is
//!   meaningless for `0` and `3`. See [`ProgressState`].
//! - **OSC 777** (`ESC ] 777 ; notify ; <title> ; <body> BEL`) - urxvt's `notify` module, the
//!   other notification convention agent CLIs emit. Only the `notify` sub-command counts;
//!   OSC 777 has other sub-commands (e.g. `precmd`) that are not attention requests.
//!
//! Everything else is ignored, including OSC 0/2 - the window title arrives through
//! `alacritty_terminal`'s own `Event::Title` on the primary pipeline (see
//! `crate::terminal::grid::TermEventSink`), and parsing it twice would just create a second copy
//! to drift.

use alacritty_terminal::vte::{Parser, Perform};

/// The ConEmu OSC 9;4 progress states, exactly as that protocol defines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// `1` - a normal, determinate progress report; [`Progress::percent`] is meaningful.
    Normal,
    /// `2` - an error state, still carrying a percentage.
    Error,
    /// `3` - work is happening but its extent is unknown; [`Progress::percent`] is `None`.
    Indeterminate,
    /// `4` - the reported work is paused.
    Paused,
}

impl ProgressState {
    /// Whether this state means "work is actively happening right now".
    ///
    /// [`ProgressState::Error`] and [`ProgressState::Paused`] are deliberately *not* busy: both
    /// describe work that has stopped advancing, which is exactly when a human's attention may
    /// be wanted, so claiming "still running" for them would be the false-positive this whole
    /// signal exists to remove.
    pub fn is_active(self) -> bool {
        matches!(self, ProgressState::Normal | ProgressState::Indeterminate)
    }
}

/// One OSC 9;4 progress report. State `0` (clear) has no [`Progress`] - it clears the stored
/// report to `None` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub state: ProgressState,
    /// The reported percentage, clamped to `0..=100`. `None` for
    /// [`ProgressState::Indeterminate`], where the protocol defines no meaningful value, and for
    /// a report whose percentage field was absent or unparseable.
    pub percent: Option<u8>,
}

/// The mutable signal state [`OscWatcher`] accumulates - separate from [`OscWatcher`] itself
/// because `vte::Parser::advance` needs `&mut P` for the `Perform` impl while it also holds
/// `&mut self`, so the parser and the thing it writes into cannot be the same value.
#[derive(Debug, Clone, Copy, Default)]
struct OscSignals {
    /// Set by any OSC 9 notification or OSC 777 `notify`; cleared by
    /// [`OscWatcher::take_attention_ping`].
    attention_pinged: bool,
    progress: Option<Progress>,
}

impl Perform for OscSignals {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(&kind) = params.first() else {
            return;
        };
        match kind {
            // OSC 9;4;<state>;<progress> is ConEmu progress; any *other* OSC 9 is an iTerm2
            // desktop notification. The `params.len() >= 2` guard matters: a bare `ESC]9BEL`
            // with no message is not a notification anyone meant to send.
            b"9" if params.get(1) == Some(&b"4".as_slice()) => {
                self.progress = parse_progress(params.get(2).copied(), params.get(3).copied());
            }
            b"9" if params.len() >= 2 => self.attention_pinged = true,
            b"777" if params.get(1) == Some(&b"notify".as_slice()) => {
                self.attention_pinged = true;
            }
            _ => {}
        }
    }
}

/// Parses the `<state>` and `<progress>` fields of an OSC 9;4 payload. Returns `None` for
/// state `0` (explicitly "clear the progress indicator") and for any state outside `0..=4`,
/// which no sender should emit and which would otherwise be silently rounded into a real state.
fn parse_progress(state: Option<&[u8]>, percent: Option<&[u8]>) -> Option<Progress> {
    let state = match parse_u8(state?)? {
        0 => return None,
        1 => ProgressState::Normal,
        2 => ProgressState::Error,
        3 => ProgressState::Indeterminate,
        4 => ProgressState::Paused,
        _ => return None,
    };
    let percent = match state {
        // The protocol defines no percentage for an indeterminate report; senders commonly put
        // a `0` there, and reporting that as "0% done" would be inventing a fact.
        ProgressState::Indeterminate => None,
        _ => percent.and_then(parse_u8).map(|percent| percent.min(100)),
    };
    Some(Progress { state, percent })
}

/// Parses an ASCII decimal parameter. Deliberately strict - a param with any non-digit byte is
/// not a number, rather than a number with the trailing junk ignored.
fn parse_u8(bytes: &[u8]) -> Option<u8> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // Saturating rather than `Option`: a sender writing `9;4;1;999` meant "a big percentage",
    // and every consumer here clamps to `0..=100` anyway.
    let mut value: u32 = 0;
    for byte in bytes {
        value = value
            .saturating_mul(10)
            .saturating_add(u32::from(byte - b'0'));
        if value > u32::from(u8::MAX) {
            return Some(u8::MAX);
        }
    }
    Some(value as u8)
}

/// A `vte::Parser` fed the same raw pty bytes as the rendering pipeline, watching only for the
/// OSC sequences that pipeline drops - see the module docs.
pub struct OscWatcher {
    parser: Parser,
    signals: OscSignals,
}

impl Default for OscWatcher {
    fn default() -> Self {
        Self {
            parser: Parser::new(),
            signals: OscSignals::default(),
        }
    }
}

impl std::fmt::Debug for OscWatcher {
    /// `vte::Parser` is not `Debug`, and the parser's internal state machine position isn't
    /// something a caller could act on anyway - only the accumulated signals are.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OscWatcher")
            .field("signals", &self.signals)
            .finish_non_exhaustive()
    }
}

impl OscWatcher {
    /// Feeds a chunk of raw pty bytes. Must be handed exactly the same slices, in the same
    /// order, as the primary parser: OSC sequences can straddle a chunk boundary, and the
    /// `vte::Parser` state machine is what carries the partial sequence across.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.signals, bytes);
    }

    /// Consumes the "a notification fired" flag: `true` if at least one OSC 9 notification or
    /// OSC 777 `notify` arrived since the last call, and clears it.
    ///
    /// Consume-on-read because these are point-in-time events, not state - the sender says "now",
    /// never "still". The caller (`crate::terminal::pane::TerminalPane`'s poll loop) is
    /// responsible for turning that instant into whatever durable state it needs; leaving the
    /// flag latched here instead would make it stick forever, since nothing in the protocol ever
    /// un-fires a notification.
    pub fn take_attention_ping(&mut self) -> bool {
        std::mem::take(&mut self.signals.attention_pinged)
    }

    /// The most recent OSC 9;4 progress report, or `None` if none has arrived or the last one
    /// was state `0` (clear). Unlike [`Self::take_attention_ping`] this is real state - the
    /// protocol's whole point is that a progress report stands until superseded or cleared - so
    /// it is not consumed on read.
    pub fn progress(&self) -> Option<Progress> {
        self.signals.progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(bytes: &[u8]) -> OscWatcher {
        let mut watcher = OscWatcher::default();
        watcher.feed(bytes);
        watcher
    }

    #[test]
    fn a_bare_osc_9_notification_pings_for_attention_once() {
        let mut watcher = watch(b"\x1b]9;Claude needs your permission\x07");
        assert!(watcher.take_attention_ping());
        assert!(
            !watcher.take_attention_ping(),
            "the ping is a point-in-time event and must not re-fire on a second read"
        );
    }

    #[test]
    fn an_st_terminated_osc_9_also_pings() {
        // OSC can be terminated by BEL *or* by ST (`ESC \`); real senders use both.
        let mut watcher = watch(b"\x1b]9;done\x1b\\");
        assert!(watcher.take_attention_ping());
    }

    #[test]
    fn osc_777_notify_pings_but_other_777_subcommands_do_not() {
        let mut watcher = watch(b"\x1b]777;notify;Gemini;waiting on you\x07");
        assert!(watcher.take_attention_ping());

        let mut watcher = watch(b"\x1b]777;precmd;0\x07");
        assert!(
            !watcher.take_attention_ping(),
            "OSC 777 has non-notification sub-commands; only `notify` is an attention request"
        );
    }

    #[test]
    fn osc_9_4_is_progress_not_a_notification() {
        let mut watcher = watch(b"\x1b]9;4;1;42\x07");
        assert_eq!(
            watcher.progress(),
            Some(Progress {
                state: ProgressState::Normal,
                percent: Some(42)
            })
        );
        assert!(
            !watcher.take_attention_ping(),
            "a progress report is not a request for the human's attention"
        );
    }

    #[test]
    fn every_progress_state_parses_to_its_documented_meaning() {
        assert_eq!(watch(b"\x1b]9;4;0;0\x07").progress(), None);
        assert_eq!(
            watch(b"\x1b]9;4;2;80\x07").progress(),
            Some(Progress {
                state: ProgressState::Error,
                percent: Some(80)
            })
        );
        assert_eq!(
            watch(b"\x1b]9;4;3;0\x07").progress(),
            Some(Progress {
                state: ProgressState::Indeterminate,
                percent: None
            }),
            "indeterminate defines no percentage - reporting the sender's filler 0 as \"0% done\" \
             would be inventing a fact"
        );
        assert_eq!(
            watch(b"\x1b]9;4;4;33\x07").progress(),
            Some(Progress {
                state: ProgressState::Paused,
                percent: Some(33)
            })
        );
    }

    #[test]
    fn only_normal_and_indeterminate_progress_count_as_active_work() {
        assert!(ProgressState::Normal.is_active());
        assert!(ProgressState::Indeterminate.is_active());
        assert!(!ProgressState::Error.is_active());
        assert!(!ProgressState::Paused.is_active());
    }

    #[test]
    fn a_clear_report_clears_an_earlier_progress() {
        let mut watcher = OscWatcher::default();
        watcher.feed(b"\x1b]9;4;1;50\x07");
        assert!(watcher.progress().is_some());
        watcher.feed(b"\x1b]9;4;0\x07");
        assert_eq!(watcher.progress(), None);
    }

    #[test]
    fn a_later_progress_report_supersedes_an_earlier_one() {
        let mut watcher = OscWatcher::default();
        watcher.feed(b"\x1b]9;4;1;10\x07");
        watcher.feed(b"\x1b]9;4;1;90\x07");
        assert_eq!(watcher.progress().and_then(|p| p.percent), Some(90));
    }

    #[test]
    fn a_percentage_above_100_is_clamped_and_a_junk_one_is_dropped() {
        assert_eq!(
            watch(b"\x1b]9;4;1;999\x07").progress().unwrap().percent,
            Some(100)
        );
        assert_eq!(
            watch(b"\x1b]9;4;1;abc\x07").progress().unwrap().percent,
            None,
            "an unparseable percentage is absent, not zero"
        );
    }

    #[test]
    fn an_out_of_range_progress_state_is_ignored_entirely() {
        assert_eq!(watch(b"\x1b]9;4;7;50\x07").progress(), None);
        assert_eq!(watch(b"\x1b]9;4\x07").progress(), None);
    }

    #[test]
    fn unrelated_osc_sequences_and_plain_text_produce_no_signal() {
        let mut watcher = watch(
            b"hello \x1b]0;some window title\x07 world\x1b]8;;https://example.com\x07link\
              \x1b]52;c;Zm9v\x07\x1b]4;1;#ff0000\x07",
        );
        assert!(!watcher.take_attention_ping());
        assert_eq!(watcher.progress(), None);
    }

    #[test]
    fn a_sequence_split_across_chunk_boundaries_still_parses() {
        // The pty hands over arbitrary chunk boundaries; the `vte::Parser` state machine is what
        // carries a half-read sequence across one, which is why the watcher owns a persistent
        // parser rather than parsing each chunk standalone.
        let mut watcher = OscWatcher::default();
        for chunk in [
            b"\x1b]9".as_slice(),
            b";4;1".as_slice(),
            b";7".as_slice(),
            b"7\x07".as_slice(),
        ] {
            watcher.feed(chunk);
        }
        assert_eq!(watcher.progress().and_then(|p| p.percent), Some(77));
    }

    #[test]
    fn a_bare_osc_9_with_no_message_is_not_a_notification() {
        let mut watcher = watch(b"\x1b]9\x07");
        assert!(!watcher.take_attention_ping());
    }
}
