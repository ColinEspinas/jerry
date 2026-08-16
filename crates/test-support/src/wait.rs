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
pub fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let expiry = Instant::now() + deadline;
    loop {
        if condition() {
            return true;
        }
        let remaining = expiry.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        thread::sleep(POLL_INTERVAL.min(remaining));
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
    use crate::{stays_false, wait_until};
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
