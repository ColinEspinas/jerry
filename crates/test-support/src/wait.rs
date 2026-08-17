//! Bounded waits for conditions only a real OS thread can satisfy — a file watcher's callback,
//! a spawned process reaching a state.
//!
//! This module is the *only* sanctioned wall-clock wait in the workspace (`docs/testing.md`).
//! Anything a GPUI executor drives waits with `cx.run_until_parked()` instead, and nothing waits
//! with a bare `thread::sleep`: a fixed sleep is simultaneously too short on a loaded machine
//! and pure dead time on an idle one.

use std::thread;
use std::time::{Duration, Instant};

/// How often `condition` is re-checked. Short enough that a satisfied condition is noticed
/// almost immediately, long enough not to spin a core while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Polls `condition` until it returns `true` or `deadline` elapses; the return value says which,
/// so the caller supplies the assertion message.
///
/// ```no_run
/// # use std::time::Duration;
/// # let watcher_fired = || true;
/// assert!(
///     test_support::wait_until(Duration::from_secs(3), watcher_fired),
///     "the watcher must observe a real file write within the tier's budget"
/// );
/// ```
pub fn wait_until(deadline: Duration, condition: impl FnMut() -> bool) -> bool {
    wait_until_every(POLL_INTERVAL, deadline, condition)
}

/// [`wait_until`] with an explicit poll interval, for a condition whose *check* perturbs the very
/// thing it measures — a probe that takes a connection slot in the server it is asking about, a
/// check that contends for the lock the subject needs. At [`POLL_INTERVAL`] such a probe measures
/// its own polling rate rather than the subject; pick an interval coarse enough that it doesn't.
///
/// Prefer plain [`wait_until`] everywhere else: a coarse interval buys nothing when the check is
/// free, and costs up to `interval` of dead time on a condition that has already been satisfied.
///
/// ```no_run
/// # use std::time::Duration;
/// # let served = || true;
/// assert!(
///     test_support::wait_until_every(Duration::from_millis(250), Duration::from_secs(20), served),
///     "the server must recover once it gives up on the clients holding every slot"
/// );
/// ```
pub fn wait_until_every(
    interval: Duration,
    deadline: Duration,
    mut condition: impl FnMut() -> bool,
) -> bool {
    let expiry = Instant::now() + deadline;
    loop {
        if condition() {
            return true;
        }
        let remaining = expiry.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        thread::sleep(interval.min(remaining));
    }
}

/// The inverse of [`wait_until`]: `true` if `condition` stayed false for the whole `window`.
///
/// For proving something is genuinely filtered out rather than merely slow to arrive — the case
/// that would otherwise be written as a bare `thread::sleep` followed by one assertion.
pub fn stays_false(window: Duration, condition: impl FnMut() -> bool) -> bool {
    !wait_until(window, condition)
}

#[cfg(test)]
mod bounded_wait_tests {
    use crate::{stays_false, wait_until, wait_until_every};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn wait_until_returns_as_soon_as_the_condition_holds() {
        let polls = AtomicUsize::new(0);
        let started = Instant::now();

        let satisfied = wait_until(Duration::from_secs(30), || {
            polls.fetch_add(1, Ordering::SeqCst) >= 2
        });

        assert!(
            satisfied,
            "the condition became true well inside the deadline"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a satisfied wait must return immediately, not sit out its deadline - it took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn wait_until_gives_up_after_the_deadline() {
        let started = Instant::now();

        let satisfied = wait_until(Duration::from_millis(50), || false);

        assert!(
            !satisfied,
            "a condition that never holds must report failure"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "the full deadline must really be waited out before giving up"
        );
    }

    #[test]
    fn wait_until_checks_the_condition_before_sleeping() {
        let started = Instant::now();

        assert!(wait_until(Duration::from_secs(30), || true));
        assert!(
            started.elapsed() < Duration::from_millis(10),
            "an already-satisfied condition must cost no wall-clock time at all"
        );
    }

    #[test]
    fn wait_until_every_polls_at_the_interval_it_was_given_rather_than_the_default() {
        let polls = AtomicUsize::new(0);
        let started = Instant::now();

        let satisfied =
            wait_until_every(Duration::from_millis(100), Duration::from_secs(30), || {
                polls.fetch_add(1, Ordering::SeqCst) >= 3
            });

        assert!(
            satisfied,
            "the condition became true well inside the deadline"
        );
        // Four checks at 100ms is at least 300ms of sleeping; at `wait_until`'s own 10ms it would
        // be ~30ms, which is what this distinguishes.
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "a coarse interval must really be waited out between checks - it took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn wait_until_every_never_sleeps_past_the_deadline() {
        let started = Instant::now();

        assert!(!wait_until_every(
            Duration::from_secs(30),
            Duration::from_millis(50),
            || false
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the interval must be clamped to what is left of the deadline, not slept in full - it \
             took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn stays_false_reports_a_condition_that_never_fires() {
        assert!(stays_false(Duration::from_millis(30), || false));
    }

    #[test]
    fn stays_false_reports_a_condition_that_does_fire() {
        let polls = AtomicUsize::new(0);

        assert!(
            !stays_false(Duration::from_secs(30), || polls
                .fetch_add(1, Ordering::SeqCst)
                >= 1),
            "a condition that becomes true inside the window must fail the quiet-period check"
        );
    }
}
