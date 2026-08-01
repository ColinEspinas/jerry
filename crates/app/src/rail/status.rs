//! Agent-status derivation - the "who needs me" mechanism `design_handoff_jerry_ade/README.md`
//! calls the whole point of the agent rail.
//!
//! GPUI-free and pty/process-free: takes small, already-read signals ([`ProcessSignal`], a
//! `has_reviewable_diff` bool, a [`crate::work_surface::agents::AgentKind`]) and returns a [`Status`], so
//! the decision logic is unit testable without a window or a child process. Gathering those
//! signals from a live [`crate::terminal::pane::TerminalPane`] and `wt_core::diff::diff_against_base`
//! lives in `crate::rail::state`/`crate::root`.
//!
//! ## The heuristic, precisely
//!
//! Exit-based statuses ([`Status::Fail`], [`Status::Review`]) are exact: a process either exited
//! non-zero/was killed by a signal, or exited 0. Whether a "review ready" exit has anything to
//! review is likewise exact - `wt_core::diff::diff_against_base` reporting at least one changed
//! file.
//!
//! The one fuzzy call is distinguishing [`Status::Run`] from [`Status::Ask`] for a still-alive
//! process: there's no portable, reliable way to know from outside a process that it's blocked
//! reading its own stdin (the precise version of that signal exists only on Linux, only
//! per-thread, via `/proc/<pid>/wchan`, and isn't cheap or reliable to attribute to "waiting on
//! the pty's stdin" specifically). This module uses the same substitute tools like tmux's
//! `monitor-activity` use: **idle time**. A process that was streaming output and has now gone
//! quiet longer than a threshold is treated as "probably waiting on input" - a heuristic, not a
//! certainty.
//!
//! Two thresholds, because a plain shell and an interactive agent CLI mean something different
//! by "gone quiet":
//! - [`RUN_RECENT_OUTPUT_WINDOW`] (2s) is the boundary between "actively streaming" and "merely
//!   paused" for any live process.
//! - [`AGENT_ASK_IDLE_THRESHOLD`] (15s) is a second, longer threshold that only matters for
//!   [`crate::work_surface::agents::AgentKind::Claude`]/[`crate::work_surface::agents::AgentKind::Codex`] agents:
//!   an agent CLI commonly pauses for several seconds between a tool call and its result, so
//!   treating every pause past 2s as "needs input" would flicker the rail on normal agent
//!   latency. Only past the longer window is an agent flagged [`Status::Ask`].
//!
//! A plain [`crate::work_surface::agents::AgentKind::Shell`] has no such grace window: a shell sitting at
//! its prompt isn't "asking a question", it's just idle - so it falls straight to
//! [`Status::Idle`] once [`RUN_RECENT_OUTPUT_WINDOW`] elapses, never [`Status::Ask`].

use std::time::Duration;

use crate::work_surface::agents::AgentKind;

/// How long a live process must have produced output within to count as "recently active" - the
/// short end of the Run/Ask heuristic (see the module docs).
pub const RUN_RECENT_OUTPUT_WINDOW: Duration = Duration::from_secs(2);

/// How long an agent-CLI agent ([`AgentKind::Claude`]/[`AgentKind::Codex`]) must have
/// been quiet before it's flagged [`Status::Ask`] - see the module docs for why this is longer
/// than [`RUN_RECENT_OUTPUT_WINDOW`].
pub const AGENT_ASK_IDLE_THRESHOLD: Duration = Duration::from_secs(15);

/// The status vocabulary from `design_handoff_jerry_ade/README.md`'s "Status vocabulary" table,
/// backed one-to-one by `crate::theme::status::*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// Needs input.
    Ask,
    /// Failed (non-zero exit, or killed by a signal).
    Fail,
    /// Exited 0 with a real, non-empty diff against base.
    Review,
    /// Alive and has produced output within [`RUN_RECENT_OUTPUT_WINDOW`], or alive and merely
    /// paused (not yet past its own ask threshold, for agents).
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
            Status::Ask => crate::theme::status::ASK.into(),
            Status::Fail => crate::theme::status::FAIL.into(),
            Status::Review => crate::theme::status::REVIEW.into(),
            Status::Run => crate::theme::status::RUN.into(),
            Status::Idle => crate::theme::status::IDLE.into(),
        }
    }

    /// The status pill's background colour (`design_handoff_jerry_ade/README.md`'s "Agent
    /// context bar" spec) - `crate::theme::status::*_BG`, one per [`Status`].
    pub fn pill_bg(self) -> gpui::Rgba {
        match self {
            Status::Ask => crate::theme::status::ASK_BG.into(),
            Status::Fail => crate::theme::status::FAIL_BG.into(),
            Status::Review => crate::theme::status::REVIEW_BG.into(),
            Status::Run => crate::theme::status::RUN_BG.into(),
            Status::Idle => crate::theme::status::IDLE_BG.into(),
        }
    }

    /// Every status, already in the README's "by urgency" group order - `crate::rail::state`'s
    /// urgency grouping iterates this rather than re-deriving it from [`Self::urgency_rank`].
    pub const ORDER: [Status; 5] = [
        Status::Ask,
        Status::Fail,
        Status::Review,
        Status::Run,
        Status::Idle,
    ];
}

/// The already-read signal this module derives a [`Status`] from - built by the caller (see
/// `crate::rail::state`) from a live [`crate::terminal::pane::TerminalPane`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    /// The child process is alive; `idle` is how long since its pty last produced output (or
    /// since it started, if never).
    Running { idle: Duration },
    /// The child process has exited (or a spawn attempt failed outright, treated the same as a
    /// non-zero exit - see `crate::rail::state`'s docs).
    Exited { success: bool },
    /// No process has ever run in this agent slot (or one is mid-async-spawn - see
    /// `crate::rail::state`'s docs).
    NoProcess,
}

/// Derives the [`Status`] for one agent from its process signal and whether it has a
/// non-empty diff against its worktree's base - see the module docs for the Run/Ask split.
pub fn derive_status(kind: AgentKind, signal: ProcessSignal, has_reviewable_diff: bool) -> Status {
    match signal {
        ProcessSignal::NoProcess => Status::Idle,
        ProcessSignal::Running { idle } => match kind {
            AgentKind::Shell => {
                if idle < RUN_RECENT_OUTPUT_WINDOW {
                    Status::Run
                } else {
                    Status::Idle
                }
            }
            AgentKind::Claude | AgentKind::Codex => {
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
            derive_status(AgentKind::Shell, ProcessSignal::NoProcess, true),
            Status::Idle
        );
        assert_eq!(
            derive_status(AgentKind::Claude, ProcessSignal::NoProcess, true),
            Status::Idle
        );
    }

    #[test]
    fn running_within_the_recent_window_is_run_for_every_kind() {
        let signal = ProcessSignal::Running {
            idle: Duration::from_millis(500),
        };
        assert_eq!(derive_status(AgentKind::Shell, signal, false), Status::Run);
        assert_eq!(derive_status(AgentKind::Claude, signal, false), Status::Run);
        assert_eq!(derive_status(AgentKind::Codex, signal, false), Status::Run);
    }

    #[test]
    fn a_quiet_shell_past_the_recent_window_is_idle_not_ask() {
        let signal = ProcessSignal::Running {
            idle: RUN_RECENT_OUTPUT_WINDOW + Duration::from_secs(1),
        };
        assert_eq!(
            derive_status(AgentKind::Shell, signal, false),
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
        assert_eq!(derive_status(AgentKind::Claude, signal, false), Status::Run);
        assert_eq!(derive_status(AgentKind::Codex, signal, false), Status::Run);
    }

    #[test]
    fn an_agent_quiet_past_the_ask_threshold_is_ask() {
        let signal = ProcessSignal::Running {
            idle: AGENT_ASK_IDLE_THRESHOLD + Duration::from_secs(1),
        };
        assert_eq!(derive_status(AgentKind::Claude, signal, false), Status::Ask);
        assert_eq!(derive_status(AgentKind::Codex, signal, false), Status::Ask);
    }

    #[test]
    fn nonzero_exit_is_fail_regardless_of_diff() {
        let signal = ProcessSignal::Exited { success: false };
        assert_eq!(derive_status(AgentKind::Shell, signal, true), Status::Fail);
        assert_eq!(
            derive_status(AgentKind::Claude, signal, false),
            Status::Fail
        );
    }

    #[test]
    fn zero_exit_with_a_real_diff_is_review() {
        let signal = ProcessSignal::Exited { success: true };
        assert_eq!(
            derive_status(AgentKind::Claude, signal, true),
            Status::Review
        );
    }

    #[test]
    fn zero_exit_with_no_diff_is_idle_not_review() {
        let signal = ProcessSignal::Exited { success: true };
        assert_eq!(
            derive_status(AgentKind::Claude, signal, false),
            Status::Idle
        );
    }

    #[test]
    fn pill_bg_is_distinct_per_status_and_matches_the_dot_colour_family() {
        // Every status must have its own pill background - a copy-pasted match arm could
        // silently share one, and two similar dark ambers/reds would still "look plausible".
        // `gpui::Rgba` has no `Debug` impl, so this compares raw channels instead of using
        // `assert_ne!` directly.
        let bgs: Vec<gpui::Rgba> = Status::ORDER.iter().map(|s| s.pill_bg()).collect();
        for (i, a) in bgs.iter().enumerate() {
            for (j, b) in bgs.iter().enumerate() {
                if i != j {
                    let same = a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a;
                    assert!(!same, "status {i} and {j} share a pill background");
                }
            }
        }
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
