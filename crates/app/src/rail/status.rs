//! Agent-status derivation - the "who needs me" mechanism `design_handoff_jerry_ade/README.md`
//! calls the whole point of the agent rail.
//!
//! GPUI-free and pty/process-free: takes small, already-read signals ([`ProcessSignal`], a
//! [`TerminalSignal`], a `has_reviewable_diff` bool, a
//! [`crate::work_surface::agents::ProcessKind`]) and returns a [`Status`], so the decision logic
//! is unit testable without a window or a child process. Gathering those signals from a live
//! [`crate::terminal::pane::TerminalPane`] and `wt_core::diff::diff_against_base` lives in
//! `crate::rail::state`/`crate::root`.
//!
//! ## The heuristic, precisely
//!
//! Exit-based statuses ([`Status::Fail`], [`Status::Review`]) are exact: a process either exited
//! non-zero/was killed by a signal, or exited 0. Whether a "review ready" exit has anything to
//! review is likewise exact - `wt_core::diff::diff_against_base` reporting at least one changed
//! file. [`Status::Review`] is further gated on
//! [`crate::work_surface::agents::ProcessKind::is_agent_session`]: a plain
//! [`crate::work_surface::agents::ProcessKind::Shell`] exiting next to an unrelated worktree
//! diff didn't do reviewable work - it's a terminal that closed, not a session that finished a turn -
//! so it reports [`Status::Idle`] instead, same as an agent session's clean exit with nothing to
//! review.
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
//! Two thresholds, because a plain shell and a real agent session
//! ([`crate::work_surface::agents::ProcessKind::is_agent_session`]) mean something different by
//! "gone quiet":
//! - [`RUN_RECENT_OUTPUT_WINDOW`] (2s) is the boundary between "actively streaming" and "merely
//!   paused" for any live process.
//! - [`AGENT_ASK_IDLE_THRESHOLD`] (15s) is a second, longer threshold that only matters for a
//!   real agent session: an agent CLI commonly pauses for several seconds between a tool call
//!   and its result, so treating every pause past 2s as "needs input" would flicker the rail on
//!   normal agent latency. Only past the longer window is an agent flagged [`Status::Ask`].
//!
//! A plain [`crate::work_surface::agents::ProcessKind::Shell`] has no such grace window: a shell
//! sitting at its prompt isn't "asking a question", it's just idle - so it falls straight to
//! [`Status::Idle`] once [`RUN_RECENT_OUTPUT_WINDOW`] elapses, never [`Status::Ask`].
//!
//! ## The second signal: what the agent says about itself (GitHub issue #239)
//!
//! Idle time is a substitute for a signal the process never sent. But real agent CLIs *do* send
//! one, into the same pty, and Jerry used to throw it away: they write a live status glyph into
//! the terminal title (OSC 0/2), and some fire real desktop notifications (OSC 9, OSC 777) and
//! progress reports (OSC 9;4). [`TerminalSignal`] carries all three, already parsed
//! (`crate::terminal::osc`) and already classified (`crate::rail::title_signal`), and it
//! *refines* the quiescence heuristic above rather than replacing it - a CLI that says nothing
//! lands on exactly the behaviour described above, unchanged.
//!
//! Two refinements, both only for a real agent session, and both narrowly scoped to the
//! [`ProcessSignal::Running`] branch (an exit is an exact fact; no title can argue with it):
//!
//! - **Attention beats the clock.** [`TitleSignal::NeedsAttention`], or an unanswered OSC 9 /
//!   OSC 777 notification, reports [`Status::Ask`] immediately. The 15s
//!   [`AGENT_ASK_IDLE_THRESHOLD`] exists *only* to compensate for not having this signal; when
//!   the agent says outright that it's blocked on the human, making the human wait out a timer
//!   built to guess at that same fact is pure added latency.
//! - **Busy beats the clock too, the other way.** [`TitleSignal::Busy`], or an active
//!   [`crate::terminal::osc::ProgressState`], holds [`Status::Run`] even past
//!   [`AGENT_ASK_IDLE_THRESHOLD`]. This fixes a real false-positive class the idle heuristic
//!   cannot fix on its own: a long tool call that produces no terminal output - a slow compile,
//!   a big download - goes quiet for minutes and gets read as "needs input" purely because of
//!   the silence, while the agent's own title glyph is still spinning.
//!
//! Everything else about a terminal signal is deliberately *not* acted on.
//! [`TitleSignal::Idle`] does not shortcut anything: an agent that finished its turn and is
//! sitting at its prompt is waiting on the human, which is what the existing threshold already
//! concludes on its own - so there is no decision left for it to improve, only a chance to be
//! wrong faster. And [`TitleSignal::Unknown`] means exactly what it says, "no information", not
//! "idle".
//!
//! A [`crate::work_surface::agents::ProcessKind::Shell`] never consults [`TerminalSignal`] at
//! all - not even to stay [`Status::Run`]. Titles are attacker-adjacent input in the mundane
//! sense: a shell prompt, a `vim` session, or a `printf '\e]0;...\a'` in someone's `.bashrc` can
//! put any glyph or word in a shell's title, and none of that makes a shell an agent session
//! that can need input. That gate is pinned by its own test below.

use std::time::Duration;

use crate::rail::title_signal::TitleSignal;
use crate::terminal::osc::Progress;
use crate::work_surface::agents::ProcessKind;

/// How long a live process must have produced output within to count as "recently active" - the
/// short end of the Run/Ask heuristic (see the module docs).
pub const RUN_RECENT_OUTPUT_WINDOW: Duration = Duration::from_secs(2);

/// How long a real agent session ([`ProcessKind::Agent`] - i.e. any
/// [`crate::work_surface::agents::AgentKind`] CLI, never a shell) must have been quiet before
/// it's flagged [`Status::Ask`] - see the module docs for why this is longer than
/// [`RUN_RECENT_OUTPUT_WINDOW`].
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

/// What the process said about itself through its own terminal, as opposed to what its silence
/// implies - see the module docs' "second signal" section (GitHub issue #239).
///
/// Built by the caller (`crate::work_surface::render::AdeApp::agent_status`) from a live
/// [`crate::terminal::pane::TerminalPane`]. [`Default`] is "the process said nothing", which is
/// both the honest starting state and what every non-agent process is handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalSignal {
    /// The classification of the process's own window title, or `None` if it never set one.
    pub title: Option<TitleSignal>,
    /// Whether an OSC 9 / OSC 777 desktop notification is outstanding - fired by the process and
    /// not yet answered by the human. See
    /// [`crate::terminal::pane::TerminalPane::has_pending_attention_ping`].
    pub attention_pinged: bool,
    /// The process's most recent OSC 9;4 progress report, if it speaks that protocol.
    pub progress: Option<Progress>,
}

impl TerminalSignal {
    /// Whether the process is explicitly asking for the human - see the module docs.
    fn wants_attention(self) -> bool {
        self.attention_pinged || self.title == Some(TitleSignal::NeedsAttention)
    }

    /// Whether the process is explicitly claiming to be working right now - see the module docs.
    ///
    /// A progress report only counts while it describes work that is actually advancing;
    /// [`crate::terminal::osc::ProgressState::is_active`] is where that line is drawn, and it
    /// deliberately excludes the error and paused states, which are exactly when a stalled
    /// agent would otherwise get to claim it's still busy forever.
    fn is_busy(self) -> bool {
        self.title == Some(TitleSignal::Busy) || self.progress.is_some_and(|p| p.state.is_active())
    }
}

/// Derives the [`Status`] for one agent from its process signal, what its own terminal claims
/// about it ([`TerminalSignal`]), and whether it has a non-empty diff against its worktree's
/// base - see the module docs for the Run/Ask split and how the two signals combine.
pub fn derive_status(
    kind: ProcessKind,
    signal: ProcessSignal,
    terminal: TerminalSignal,
    has_reviewable_diff: bool,
) -> Status {
    match signal {
        ProcessSignal::NoProcess => Status::Idle,
        ProcessSignal::Running { idle } => {
            if kind.is_agent_session() {
                // Both of these deliberately outrank the idle clock in both directions - see the
                // module docs' "second signal" section. `wants_attention` is checked first: an
                // agent that is both spinning a busy glyph and has an unanswered notification
                // out is one that needs the human *now* and happens to still be rendering.
                if terminal.wants_attention() {
                    Status::Ask
                } else if terminal.is_busy() || idle < AGENT_ASK_IDLE_THRESHOLD {
                    Status::Run
                } else {
                    Status::Ask
                }
            } else if idle < RUN_RECENT_OUTPUT_WINDOW {
                Status::Run
            } else {
                Status::Idle
            }
        }
        ProcessSignal::Exited { success } => {
            if success {
                // A worktree diff sitting around when a plain shell happens to exit isn't
                // this shell's doing - it's not a session that did reviewable work, it's a
                // terminal that closed. Only a real agent session's successful exit means
                // "review ready" (see `ProcessKind::is_agent_session`'s docs).
                if kind.is_agent_session() && has_reviewable_diff {
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
    use crate::terminal::osc::ProgressState;

    /// The "this process said nothing through its terminal" signal - what every pre-existing
    /// test below means, and exactly what a CLI that sets no title and sends no OSC produces.
    fn quiet() -> TerminalSignal {
        TerminalSignal::default()
    }

    /// A [`TerminalSignal`] carrying only a classified title - the common real case, since
    /// every agent CLI observed sets a title and none of them sent an OSC 9 notification.
    fn titled(title: TitleSignal) -> TerminalSignal {
        TerminalSignal {
            title: Some(title),
            ..TerminalSignal::default()
        }
    }

    #[test]
    fn no_process_is_idle_regardless_of_kind_or_diff() {
        assert_eq!(
            derive_status(ProcessKind::Shell, ProcessSignal::NoProcess, quiet(), true),
            Status::Idle
        );
        assert_eq!(
            derive_status(
                ProcessKind::claude(),
                ProcessSignal::NoProcess,
                quiet(),
                true
            ),
            Status::Idle
        );
    }

    #[test]
    fn running_within_the_recent_window_is_run_for_every_kind() {
        let signal = ProcessSignal::Running {
            idle: Duration::from_millis(500),
        };
        assert_eq!(
            derive_status(ProcessKind::Shell, signal, quiet(), false),
            Status::Run
        );
        assert_eq!(
            derive_status(ProcessKind::claude(), signal, quiet(), false),
            Status::Run
        );
        assert_eq!(
            derive_status(ProcessKind::codex(), signal, quiet(), false),
            Status::Run
        );
    }

    #[test]
    fn a_quiet_shell_past_the_recent_window_is_idle_not_ask() {
        let signal = ProcessSignal::Running {
            idle: RUN_RECENT_OUTPUT_WINDOW + Duration::from_secs(1),
        };
        assert_eq!(
            derive_status(ProcessKind::Shell, signal, quiet(), false),
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
            derive_status(ProcessKind::claude(), signal, quiet(), false),
            Status::Run
        );
        assert_eq!(
            derive_status(ProcessKind::codex(), signal, quiet(), false),
            Status::Run
        );
    }

    #[test]
    fn a_busy_title_keeps_a_long_quiet_agent_on_run_instead_of_ask() {
        // The real false-positive class this signal exists to fix (GitHub issue #239), and one
        // observed live: Claude Code kept its `\u{25d0}`/`\u{25d1}` spinner animating through a
        // 9-second run of `sleep 3` tool calls that wrote nothing at all to the terminal. Purely
        // by idle time that agent is "needs input"; by its own title it is plainly still working.
        let long_quiet = ProcessSignal::Running {
            idle: AGENT_ASK_IDLE_THRESHOLD * 20,
        };
        assert_eq!(
            derive_status(ProcessKind::claude(), long_quiet, quiet(), false),
            Status::Ask,
            "baseline: with no terminal signal, silence alone still means Ask"
        );
        assert_eq!(
            derive_status(
                ProcessKind::claude(),
                long_quiet,
                titled(TitleSignal::Busy),
                false
            ),
            Status::Run,
            "an agent whose own title says it is working must not be reported as needing input"
        );
    }

    #[test]
    fn an_active_progress_report_also_keeps_a_long_quiet_agent_on_run() {
        let long_quiet = ProcessSignal::Running {
            idle: AGENT_ASK_IDLE_THRESHOLD * 20,
        };
        for state in [ProgressState::Normal, ProgressState::Indeterminate] {
            let terminal = TerminalSignal {
                progress: Some(Progress {
                    state,
                    percent: Some(40),
                }),
                ..TerminalSignal::default()
            };
            assert_eq!(
                derive_status(ProcessKind::claude(), long_quiet, terminal, false),
                Status::Run,
                "{state:?} is work actually advancing"
            );
        }
        // A stalled or paused report is exactly when the human may be wanted, so it must not buy
        // the process an indefinite "still running" - the idle clock takes over again.
        for state in [ProgressState::Error, ProgressState::Paused] {
            let terminal = TerminalSignal {
                progress: Some(Progress {
                    state,
                    percent: Some(40),
                }),
                ..TerminalSignal::default()
            };
            assert_eq!(
                derive_status(ProcessKind::claude(), long_quiet, terminal, false),
                Status::Ask,
                "{state:?} must not be able to claim the agent is still working"
            );
        }
    }

    #[test]
    fn a_needs_attention_title_reaches_ask_long_before_the_idle_threshold() {
        // The 15s threshold exists only because Jerry used to have no better signal. When the
        // agent says outright that it's blocked on the human, waiting the timer out is pure
        // added latency.
        let barely_quiet = ProcessSignal::Running {
            idle: Duration::from_millis(100),
        };
        assert_eq!(
            derive_status(ProcessKind::claude(), barely_quiet, quiet(), false),
            Status::Run,
            "baseline: this agent is nowhere near the ask threshold"
        );
        assert_eq!(
            derive_status(
                ProcessKind::claude(),
                barely_quiet,
                titled(TitleSignal::NeedsAttention),
                false
            ),
            Status::Ask
        );
    }

    #[test]
    fn an_unanswered_osc_notification_reaches_ask_long_before_the_idle_threshold() {
        let barely_quiet = ProcessSignal::Running {
            idle: Duration::from_millis(100),
        };
        let terminal = TerminalSignal {
            attention_pinged: true,
            ..TerminalSignal::default()
        };
        assert_eq!(
            derive_status(ProcessKind::claude(), barely_quiet, terminal, false),
            Status::Ask
        );
    }

    #[test]
    fn attention_outranks_busy_when_an_agent_claims_both() {
        // An agent that is both spinning a busy glyph and has an unanswered notification out is
        // one that needs the human now and happens to still be rendering - see the module docs.
        let terminal = TerminalSignal {
            title: Some(TitleSignal::Busy),
            attention_pinged: true,
            progress: Some(Progress {
                state: ProgressState::Normal,
                percent: Some(10),
            }),
        };
        let signal = ProcessSignal::Running {
            idle: Duration::from_millis(100),
        };
        assert_eq!(
            derive_status(ProcessKind::claude(), signal, terminal, false),
            Status::Ask
        );
    }

    #[test]
    fn an_idle_or_unknown_title_changes_nothing_at_all() {
        // Neither is allowed to shortcut a decision: `Idle` reaches the same answer the existing
        // threshold already reaches on its own, and `Unknown` means "no information", never
        // "idle". Checked on both sides of the ask threshold so a wrong shortcut in either
        // direction would show up.
        for idle in [Duration::from_millis(100), AGENT_ASK_IDLE_THRESHOLD * 2] {
            let signal = ProcessSignal::Running { idle };
            let baseline = derive_status(ProcessKind::claude(), signal, quiet(), false);
            for title in [TitleSignal::Idle, TitleSignal::Unknown] {
                assert_eq!(
                    derive_status(ProcessKind::claude(), signal, titled(title), false),
                    baseline,
                    "{title:?} at idle={idle:?} must not change the answer"
                );
            }
        }
    }

    #[test]
    fn a_shell_never_reaches_ask_no_matter_how_agent_like_its_title_looks() {
        // A shell prompt, a `vim` session, or a stray `printf '\e]0;...\a'` in someone's
        // `.bashrc` can put any glyph or word in a shell's title. None of that makes a shell an
        // agent session that can need input - see the module docs. This also covers the reverse
        // direction: a busy-looking title must not hold a shell on `Run` either.
        let every_signal = [
            titled(TitleSignal::NeedsAttention),
            titled(TitleSignal::Busy),
            titled(TitleSignal::Idle),
            titled(TitleSignal::Unknown),
            TerminalSignal {
                attention_pinged: true,
                ..TerminalSignal::default()
            },
            TerminalSignal {
                title: Some(TitleSignal::NeedsAttention),
                attention_pinged: true,
                progress: Some(Progress {
                    state: ProgressState::Normal,
                    percent: Some(50),
                }),
            },
        ];
        for terminal in every_signal {
            assert_eq!(
                derive_status(
                    ProcessKind::Shell,
                    ProcessSignal::Running {
                        idle: AGENT_ASK_IDLE_THRESHOLD * 2
                    },
                    terminal,
                    false
                ),
                Status::Idle,
                "a quiet shell stays Idle regardless of {terminal:?}"
            );
            assert_eq!(
                derive_status(
                    ProcessKind::Shell,
                    ProcessSignal::Running {
                        idle: Duration::from_millis(100)
                    },
                    terminal,
                    false
                ),
                Status::Run,
                "a freshly-active shell stays Run regardless of {terminal:?}"
            );
        }
    }

    #[test]
    fn a_terminal_signal_never_overrides_an_exit_or_a_missing_process() {
        // An exit is an exact fact - a stale title glyph from just before the process died must
        // not be able to argue with it.
        let shouting = TerminalSignal {
            title: Some(TitleSignal::Busy),
            attention_pinged: true,
            progress: Some(Progress {
                state: ProgressState::Normal,
                percent: Some(50),
            }),
        };
        assert_eq!(
            derive_status(
                ProcessKind::claude(),
                ProcessSignal::Exited { success: false },
                shouting,
                false
            ),
            Status::Fail
        );
        assert_eq!(
            derive_status(
                ProcessKind::claude(),
                ProcessSignal::Exited { success: true },
                shouting,
                true
            ),
            Status::Review
        );
        assert_eq!(
            derive_status(
                ProcessKind::claude(),
                ProcessSignal::NoProcess,
                shouting,
                true
            ),
            Status::Idle
        );
    }

    #[test]
    fn an_agent_quiet_past_the_ask_threshold_is_ask() {
        let signal = ProcessSignal::Running {
            idle: AGENT_ASK_IDLE_THRESHOLD + Duration::from_secs(1),
        };
        assert_eq!(
            derive_status(ProcessKind::claude(), signal, quiet(), false),
            Status::Ask
        );
        assert_eq!(
            derive_status(ProcessKind::codex(), signal, quiet(), false),
            Status::Ask
        );
    }

    #[test]
    fn nonzero_exit_is_fail_regardless_of_diff() {
        let signal = ProcessSignal::Exited { success: false };
        assert_eq!(
            derive_status(ProcessKind::Shell, signal, quiet(), true),
            Status::Fail
        );
        assert_eq!(
            derive_status(ProcessKind::claude(), signal, quiet(), false),
            Status::Fail
        );
    }

    #[test]
    fn zero_exit_with_a_real_diff_is_review() {
        let signal = ProcessSignal::Exited { success: true };
        assert_eq!(
            derive_status(ProcessKind::claude(), signal, quiet(), true),
            Status::Review
        );
    }

    #[test]
    fn a_shell_zero_exit_with_a_real_diff_is_idle_not_review() {
        // A plain shell exiting next to an unrelated worktree diff isn't a session that
        // finished reviewable work - it's a terminal that closed. Only a real agent session's
        // clean exit means "review ready" (see `ProcessKind::is_agent_session`'s docs).
        let signal = ProcessSignal::Exited { success: true };
        assert_eq!(
            derive_status(ProcessKind::Shell, signal, quiet(), true),
            Status::Idle,
            "a shell isn't an agent session - its exit can't be \"review ready\""
        );
    }

    #[test]
    fn zero_exit_with_no_diff_is_idle_not_review() {
        let signal = ProcessSignal::Exited { success: true };
        assert_eq!(
            derive_status(ProcessKind::claude(), signal, quiet(), false),
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
