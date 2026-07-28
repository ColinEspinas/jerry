//! Real session-status derivation - the "who needs me" mechanism
//! `design_handoff_jerry_ade/README.md` calls the whole point of the session rail.
//!
//! This module is deliberately GPUI-free and pty/process-free: it takes small, already-read
//! signals ([`ProcessSignal`], a `has_reviewable_diff` bool, a [`crate::sessions::SessionKind`])
//! and returns a [`Status`], so the interesting decision logic can be unit tested directly
//! (see the tests below) without spinning up a real window or a real child process. The
//! *gathering* of those signals from a real [`crate::terminal_pane::TerminalPane`] and a real
//! `wt_core::diff::diff_against_base` call lives in `crate::rail`/`crate::root`.
//!
//! ## The heuristic, precisely
//!
//! Exit-based statuses ([`Status::Fail`], [`Status::Review`]) are exact, not heuristic: a
//! process either exited with a non-zero code (or was killed by a signal - `pty-core`'s
//! `ExitStatus::success()` is `false` for both) or it exited 0. Whether a "review ready" exit
//! actually has anything to review is likewise exact - it's `wt_core::diff::diff_against_base`
//! reporting at least one changed file, a real diff.
//!
//! The one genuinely fuzzy call is distinguishing [`Status::Run`] from [`Status::Ask`] for a
//! still-*alive* process: there is no portable, reliable way to know a process is truly
//! blocked reading its own stdin from outside it. The real, precise version of that signal
//! exists only on Linux, only per-thread, and only by parsing `/proc/<pid>/wchan` (or
//! `/proc/<pid>/status`'s `State: S (sleeping)` plus a stack-trace heuristic) - both
//! kernel-version-dependent in exact format and not something a pty's *parent* process can
//! cheaply and reliably attribute to "waiting on the pty's stdin specifically" versus "waiting
//! on literally anything else" (a network read, a mutex, a timer). Real tools that solve this
//! triage problem (tmux's `monitor-activity`, many CI dashboards) use the same pragmatic
//! substitute this module uses instead: **idle time**. A process that was streaming output and
//! has now gone quiet for longer than a threshold is treated as "probably waiting on input" -
//! a genuine heuristic, not a certainty, and documented as one everywhere it's used.
//!
//! Two thresholds, not one, because a plain shell and an interactive agent CLI mean something
//! different by "gone quiet":
//! - [`RUN_RECENT_OUTPUT_WINDOW`] (2s) is the boundary between "actively streaming" and
//!   "merely paused" for *any* live process - this is the "produced output recently" signal
//!   named in this feature's own spec.
//! - [`AGENT_ASK_IDLE_THRESHOLD`] (15s) is the *second*, longer threshold that only matters
//!   for [`crate::sessions::SessionKind::Claude`]/[`crate::sessions::SessionKind::Codex`]
//!   sessions: an agent CLI commonly pauses for several seconds between a tool call and its
//!   result, or while "thinking", so treating every pause past 2s as "needs input" would
//!   flicker the rail constantly on completely normal agent latency. Only once a session has
//!   been quiet for the *longer* window is it actually flagged [`Status::Ask`].
//!
//! A plain [`crate::sessions::SessionKind::Shell`] has no equivalent long grace window: a
//! shell sitting at its prompt isn't "asking a question", it's just idle (per the design's own
//! `Status::Idle` definition, "a shell tab that's just sitting there") - so a shell falls
//! straight to [`Status::Idle`] once [`RUN_RECENT_OUTPUT_WINDOW`] has elapsed, never
//! [`Status::Ask`].

use std::time::Duration;

use crate::sessions::SessionKind;

/// How long a *live* process must have produced output within to count as "recently active"
/// (the `Run` label's own condition, and the short end of the Run/Ask heuristic - see the
/// module docs). Matches this feature's own spec ("within the last ~2s").
pub const RUN_RECENT_OUTPUT_WINDOW: Duration = Duration::from_secs(2);

/// How long an agent-CLI session ([`SessionKind::Claude`]/[`SessionKind::Codex`]) must have
/// been quiet (no new pty output) before it's flagged [`Status::Ask`] - a real, but
/// heuristic, "probably waiting on stdin" signal; see the module docs for why this exists and
/// why it's longer than [`RUN_RECENT_OUTPUT_WINDOW`].
pub const AGENT_ASK_IDLE_THRESHOLD: Duration = Duration::from_secs(15);

/// The exact status vocabulary from `design_handoff_jerry_ade/README.md`'s "Status
/// vocabulary" table - used nowhere else in the app (colour is reserved for status and
/// diffs), backed one-to-one by `crate::theme::status::*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// Needs input.
    Ask,
    /// Failed (non-zero exit, or killed by a signal).
    Fail,
    /// Exited 0 with a real, non-empty diff against base.
    Review,
    /// Alive and has produced output within [`RUN_RECENT_OUTPUT_WINDOW`], or alive and merely
    /// paused (not yet past its own ask threshold, for agent sessions).
    Run,
    /// No process running, or a shell that's just sitting there.
    Idle,
}

impl Status {
    /// Rank used to sort the "by urgency" group order from
    /// `design_handoff_jerry_ade/README.md`: `Needs input → Failed → Review ready → Running →
    /// Idle`. Lower sorts first.
    pub fn urgency_rank(self) -> u8 {
        match self {
            Status::Ask => 0,
            Status::Fail => 1,
            Status::Review => 2,
            Status::Run => 3,
            Status::Idle => 4,
        }
    }

    /// The label text from the README's status table.
    pub fn label(self) -> &'static str {
        match self {
            Status::Ask => "Needs input",
            Status::Fail => "Failed",
            Status::Review => "Review ready",
            Status::Run => "Running",
            Status::Idle => "Idle",
        }
    }

    /// The status dot / left-edge colour, from `crate::theme::status`.
    pub fn color(self) -> gpui::Rgba {
        match self {
            Status::Ask => crate::theme::status::ASK,
            Status::Fail => crate::theme::status::FAIL,
            Status::Review => crate::theme::status::REVIEW,
            Status::Run => crate::theme::status::RUN,
            Status::Idle => crate::theme::status::IDLE,
        }
    }

    /// Every status, already in the README's "by urgency" group order - `crate::rail`'s
    /// urgency grouping iterates this rather than re-deriving the order from
    /// [`Self::urgency_rank`] at each call site.
    pub const ORDER: [Status; 5] = [
        Status::Ask,
        Status::Fail,
        Status::Review,
        Status::Run,
        Status::Idle,
    ];
}

/// The real, already-read signal this module derives a [`Status`] from - built by the caller
/// (see `crate::rail`) from a live [`crate::terminal_pane::TerminalPane`], never fabricated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    /// The child process is alive; `idle` is how long it's been since its pty last produced
    /// output (or since it started, if it never has yet).
    Running { idle: Duration },
    /// The child process has exited (or a spawn attempt failed outright, treated the same as
    /// a non-zero exit - see `crate::rail`'s docs on why).
    Exited { success: bool },
    /// No process has ever run in this session slot (or one is still in the middle of an
    /// async spawn - see `crate::rail`'s docs).
    NoProcess,
}

/// Derive the real [`Status`] for one session from its already-read process signal and
/// whether it has a real, non-empty diff against its worktree's base - see the module docs
/// for the full reasoning behind the Run/Ask split.
pub fn derive_status(
    kind: SessionKind,
    signal: ProcessSignal,
    has_reviewable_diff: bool,
) -> Status {
    match signal {
        ProcessSignal::NoProcess => Status::Idle,
        ProcessSignal::Running { idle } => match kind {
            SessionKind::Shell => {
                if idle < RUN_RECENT_OUTPUT_WINDOW {
                    Status::Run
                } else {
                    Status::Idle
                }
            }
            SessionKind::Claude | SessionKind::Codex => {
                if idle < AGENT_ASK_IDLE_THRESHOLD {
                    Status::Run
                } else {
                    Status::Ask
                }
            }
        },
        ProcessSignal::Exited { success } => {
            if success {
                if has_reviewable_diff {
                    Status::Review
                } else {
                    Status::Idle
                }
            } else {
                Status::Fail
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_process_is_idle_regardless_of_kind_or_diff() {
        assert_eq!(
            derive_status(SessionKind::Shell, ProcessSignal::NoProcess, true),
            Status::Idle
        );
        assert_eq!(
            derive_status(SessionKind::Claude, ProcessSignal::NoProcess, true),
            Status::Idle
        );
    }

    #[test]
    fn running_within_the_recent_window_is_run_for_every_kind() {
        let signal = ProcessSignal::Running {
            idle: Duration::from_millis(500),
        };
        assert_eq!(
            derive_status(SessionKind::Shell, signal, false),
            Status::Run
        );
        assert_eq!(
            derive_status(SessionKind::Claude, signal, false),
            Status::Run
        );
        assert_eq!(
            derive_status(SessionKind::Codex, signal, false),
            Status::Run
        );
    }

    #[test]
    fn a_quiet_shell_past_the_recent_window_is_idle_not_ask() {
        let signal = ProcessSignal::Running {
            idle: RUN_RECENT_OUTPUT_WINDOW + Duration::from_secs(1),
        };
        assert_eq!(
            derive_status(SessionKind::Shell, signal, false),
            Status::Idle,
            "a shell sitting at its prompt is idle, not \"needs input\" - it isn't asking anything"
        );
    }

    #[test]
    fn an_agent_paused_between_the_two_thresholds_is_still_run() {
        // Past the "recent" window but not yet past the longer agent-specific ask threshold:
        // this is the whole reason the second threshold exists (see the module docs) - normal
        // tool-call/thinking latency must not flicker to "needs input".
        let signal = ProcessSignal::Running {
            idle: RUN_RECENT_OUTPUT_WINDOW + Duration::from_secs(1),
        };
        assert_eq!(
            derive_status(SessionKind::Claude, signal, false),
            Status::Run
        );
        assert_eq!(
            derive_status(SessionKind::Codex, signal, false),
            Status::Run
        );
    }

    #[test]
    fn an_agent_quiet_past_the_ask_threshold_is_ask() {
        let signal = ProcessSignal::Running {
            idle: AGENT_ASK_IDLE_THRESHOLD + Duration::from_secs(1),
        };
        assert_eq!(
            derive_status(SessionKind::Claude, signal, false),
            Status::Ask
        );
        assert_eq!(
            derive_status(SessionKind::Codex, signal, false),
            Status::Ask
        );
    }

    #[test]
    fn nonzero_exit_is_fail_regardless_of_diff() {
        let signal = ProcessSignal::Exited { success: false };
        assert_eq!(
            derive_status(SessionKind::Shell, signal, true),
            Status::Fail
        );
        assert_eq!(
            derive_status(SessionKind::Claude, signal, false),
            Status::Fail
        );
    }

    #[test]
    fn zero_exit_with_a_real_diff_is_review() {
        let signal = ProcessSignal::Exited { success: true };
        assert_eq!(
            derive_status(SessionKind::Claude, signal, true),
            Status::Review
        );
    }

    #[test]
    fn zero_exit_with_no_diff_is_idle_not_review() {
        let signal = ProcessSignal::Exited { success: true };
        assert_eq!(
            derive_status(SessionKind::Claude, signal, false),
            Status::Idle
        );
    }

    #[test]
    fn urgency_order_matches_the_readme_exactly() {
        let ranks: Vec<u8> = Status::ORDER.iter().map(|s| s.urgency_rank()).collect();
        assert_eq!(ranks, vec![0, 1, 2, 3, 4]);
        assert_eq!(
            Status::ORDER,
            [
                Status::Ask,
                Status::Fail,
                Status::Review,
                Status::Run,
                Status::Idle
            ]
        );
    }
}
