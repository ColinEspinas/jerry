//! The poll: when Jerry reads a provider's budget, what stops it reading too often, and how a
//! real result becomes state.
//!
//! Every network call here runs on `cx.background_executor()` and mutates [`AdeApp`] only
//! afterwards, from inside the `this.update(...)` closure - the same discipline
//! `crate::review::flow` and `crate::updater::flow` already follow.

use std::time::Instant;

use super::fetch;
use super::state::{lock_shared_budget, polling_enabled, Provider};
use super::*;

/// How often the loop wakes up to *consider* polling. Much shorter than
/// [`super::state::POLL_INTERVAL`], which is the real cadence a single provider is read at, and
/// deliberately so: the heartbeat is what makes "you just opened the first agent of the session"
/// and "you just logged into a provider's CLI" show up in seconds rather than in minutes, without
/// this module having to reach into every code path that spawns an agent. A wake that finds
/// nothing due does no I/O at all.
const POLL_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

impl AdeApp {
    /// Starts the background budget poll - called once, from `Self::new_with_settings`, next to
    /// the update-check loop it sits beside.
    ///
    /// Does nothing at all when [`polling_enabled`] says so - under `cfg(test)`, or with
    /// `JERRY_DISABLE_PROVIDER_POLL` set; see [`super::state::DISABLE_PROVIDER_POLL_ENV`] for why
    /// a test suite must never spend a real person's rate-limit allowance and why the `cfg` alone
    /// is not enough to promise that.
    ///
    /// Every window starts one of these, and that is safe *because* the state it polls against is
    /// process-global ([`super::state::shared_budget`]): the second window's heartbeat finds the
    /// first window's poll already recent and does nothing but copy the result onto its own
    /// screen. The loop is per window; the requests are per process.
    pub(crate) fn start_budget_poll_loop(&mut self, cx: &mut Context<Self>) {
        if !polling_enabled() {
            return;
        }
        self.poll_budgets_if_due(cx);
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(POLL_HEARTBEAT).await;
            let alive = this.update(cx, |this, cx| {
                this.poll_budgets_if_due(cx);
                // Whatever *another* window's poll landed since the last heartbeat belongs on
                // this window's screen too - it is the same account's budget, read once.
                this.sync_budget_from_shared(cx);
            });
            if alive.is_err() {
                break;
            }
        });
        self._budget_poll_task = Some(task);
    }

    /// One heartbeat: read every provider whose turn it is.
    ///
    /// **Nothing is polled unless a budget readout is actually on screen in this window.** The
    /// readout lives in an agent pane and the popover is reachable only from one (§4u′'s accepted
    /// trade-off), so the test is the pane that is *selected right now*
    /// ([`crate::work_surface::agents::Agents::active`]) rather than merely one existing
    /// somewhere among this window's tabs: a window sitting on a shell tab has nowhere to display
    /// a budget even if a Claude tab is open behind it, and a background HTTP request whose
    /// result cannot be seen is exactly the "telemetry about telemetry" §2 rejects. It also means
    /// the common case of Jerry sitting open on a terminal costs a provider nothing.
    ///
    /// Both providers are polled once any agent pane is selected, not only the one that pane
    /// spends: the popover that pane opens lists every provider, and a row that reads `checking…`
    /// because nothing had ever fetched it would be a worse answer than the real one.
    ///
    /// OS window activation is deliberately **not** part of this test. It is a real signal this
    /// app already tracks (`crate::root::AdeApp::window_active`), but it is delivered by an
    /// event that can be missed - a window that saw a deactivate and never the matching activate
    /// would stop polling for the rest of the session with nothing on screen to explain it. The
    /// selected tab is in-app state that only a real user action changes, so it cannot get stuck.
    pub(crate) fn poll_budgets_if_due(&mut self, cx: &mut Context<Self>) {
        if !self.a_budget_readout_is_on_screen() {
            return;
        }
        for provider in Provider::ALL {
            // Whether this provider is actually *due* is [`super::state::ProviderBudget::
            // may_poll_now`]'s question, asked against the process-global state with its lock
            // held - a second copy of the cadence rule here would be a second place for it to
            // drift, and one that could not see another window's poll anyway.
            self.refresh_provider_budget(provider, false, cx);
        }
    }

    /// Whether this window currently shows a surface a budget can be read from - the selected
    /// pane spends a provider. See [`Self::poll_budgets_if_due`] for why this is the selected
    /// pane rather than any open one.
    fn a_budget_readout_is_on_screen(&self) -> bool {
        self.agents
            .active()
            .is_some_and(|agent| super::state::provider_of(agent.kind).is_some())
    }

    /// Copies the process-global budget onto this window, and redraws only if it really changed.
    ///
    /// This is how a window that did not fire the request still shows its result: one poll per
    /// process, N windows rendering it. The `notify` is conditional because this runs on every
    /// heartbeat of every window, and an unconditional one would repaint the whole window every
    /// 15 seconds for nothing.
    pub(crate) fn sync_budget_from_shared(&mut self, cx: &mut Context<Self>) {
        let shared = lock_shared_budget().clone();
        if shared != self.budget {
            self.budget = shared;
            cx.notify();
        }
    }

    /// The popover's `Refresh`: a real, manual read of every provider, held to
    /// [`super::state::MANUAL_REFRESH_FLOOR`] per provider.
    pub(crate) fn refresh_all_budgets(&mut self, cx: &mut Context<Self>) {
        for provider in Provider::ALL {
            self.refresh_provider_budget(provider, true, cx);
        }
    }

    /// One provider's read. `manual` marks a click (`Refresh`, or a failed provider's `Retry`),
    /// which is the only kind held to the manual floor.
    ///
    /// Silently does nothing when this provider may not be polled right now - a click inside the
    /// floor, or a second click while a request is already open. Dropping it is deliberate:
    /// queueing would turn an impatient double-click into two requests against an endpoint whose
    /// own limiter is the reason this cadence exists.
    ///
    /// # The test guard lives here, not only on the timer
    ///
    /// This is the single choke point every read passes through: the background loop, the
    /// popover's `Refresh` and a failed row's `Retry` all arrive here, and only the first of the
    /// three goes anywhere near [`AdeApp::start_budget_poll_loop`]'s own [`polling_enabled`]
    /// check. A test that drove either control - a click test on the popover, a render test that
    /// happened to fire the handler - would otherwise read the developer's own OAuth credential
    /// off disk and send it to a real provider, which is precisely what that gate exists to
    /// prevent. Gating the loop alone made the promise true by accident of what the suite happens
    /// to call today; gating here makes it true by construction.
    ///
    /// # The claim is against the process, not against this window
    ///
    /// [`super::state::BudgetState::claim_poll`] runs on the process-global
    /// [`super::state::shared_budget`], so the interval, the manual floor and the single-flight
    /// guard hold across every open window rather than once per window - see that function's own
    /// docs for why an endpoint that has already answered `429` makes that the difference between
    /// a rule and a suggestion.
    pub(crate) fn refresh_provider_budget(
        &mut self,
        provider: Provider,
        manual: bool,
        cx: &mut Context<Self>,
    ) {
        if !polling_enabled() {
            return;
        }
        let now = Instant::now();
        {
            let mut shared = lock_shared_budget();
            if !shared.claim_poll(provider, manual, now) {
                return;
            }
            self.budget = shared.clone();
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { fetch::read_provider_catching_panics(provider) })
                .await;
            // Landed in the process-global state *first*, and outside the window update: this
            // window may be gone by now (closed mid-request), and a result that never reaches
            // the shared state would leave its single-flight guard set for every *other*
            // window too. The state that decides whether anyone may poll again must not depend
            // on the survival of whichever window happened to fire this one.
            lock_shared_budget().apply_read(provider, read, Instant::now());
            let _ = this.update(cx, |this, cx| {
                this.sync_budget_from_shared(cx);
            });
        })
        .detach();
    }
}

/// Real coverage for the state machine the poll drives, without touching the network.
#[cfg(test)]
mod budget_flow_tests {
    use super::super::state::{shared_budget, ProviderSnapshot, POLLING_ENABLED};
    use super::*;
    use crate::budget::fetch::ProviderRead;
    use crate::budget::state::BudgetWindow;
    use crate::root::focus::palette_focus_tests::open_test_app;
    use gpui::TestAppContext;

    /// One window of `used` percent - the direction every budget number runs in.
    fn snapshot(used: f32) -> ProviderSnapshot {
        ProviderSnapshot {
            windows: vec![BudgetWindow {
                label: "5h".to_string(),
                used_percent: used,
                resets_at: None,
            }],
        }
    }

    /// The poll must never run itself in a test build - it reads a real credential off the
    /// developer's disk and sends it to a real provider. Asserted at *compile* time rather than in
    /// a `#[test]` body: a run-time assertion can only fail after the suite has already had the
    /// chance to make the call it is guarding against, whereas this one refuses to build a test
    /// binary in which polling is on.
    ///
    /// This covers *this* crate's test targets only, which is exactly why
    /// [`super::state::DISABLE_PROVIDER_POLL_ENV`] exists beside it - see
    /// `the_environment_kill_switch_disables_polling_in_any_build` in `crate::budget::state`.
    const _: () = assert!(
        !POLLING_ENABLED,
        "a test run must never spend a real person's provider budget"
    );

    /// And the guard as *behaviour*, on the path a click really takes: the popover's `Refresh`
    /// reaches [`AdeApp::refresh_provider_budget`] without going near the background loop, so the
    /// constant above would not save a click test on its own. Nothing is even attempted here -
    /// `last_attempt` staying `None` is the proof no request was started.
    #[gpui::test]
    fn a_manual_refresh_starts_no_request_at_all_in_a_test_build(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, cx| {
            app.refresh_all_budgets(cx);
            app.refresh_provider_budget(Provider::Claude, true, cx);
            for provider in Provider::ALL {
                assert_eq!(
                    app.budget.get(provider).last_attempt,
                    None,
                    "{provider:?} must not have been read - a test must never send a real \
                     developer's OAuth credential to a real endpoint"
                );
                assert!(
                    !app.budget.get(provider).in_flight,
                    "{provider:?} must not have a request open either"
                );
            }
        });
    }

    /// The heartbeat must not touch the network for a window whose selected pane is a shell -
    /// there is nowhere to render a budget, and a request whose result cannot be seen is exactly
    /// the telemetry §2 rejects.
    #[gpui::test]
    fn a_window_showing_no_agent_pane_polls_nothing(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            assert!(
                !app.agents.iter().any(|agent| agent.kind.is_agent_session()),
                "premise: the startup pane is a shell, not an agent session"
            );
            assert!(
                !app.a_budget_readout_is_on_screen(),
                "so nothing on screen could show a budget"
            );
            app.poll_budgets_if_due(cx);
            for provider in Provider::ALL {
                assert_eq!(
                    app.budget.get(provider).last_attempt,
                    None,
                    "{provider:?} must not have been polled - nothing on screen could show it"
                );
            }
        });
    }

    /// The one shared budget really does reach a second window's own screen. Without this, making
    /// the poll process-global would have fixed the request rate by leaving every window but one
    /// permanently on `checking…`, which is a worse readout than the one it replaced.
    ///
    /// Writes the process-global state directly, which is safe precisely because polling is off
    /// in this build: nothing else in the suite reads or writes it (every other test drives a
    /// local [`super::super::state::BudgetState`]), so there is no other test for this one to
    /// race.
    #[gpui::test]
    fn a_second_window_renders_the_budget_the_first_ones_poll_fetched(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (first, cx) = open_test_app(cx, repo.path().to_path_buf());
        let (second, cx) = open_test_app(cx, repo.path().to_path_buf());

        // Exactly what the first window's poll does: claim the process-wide right to read, then
        // land the result on the shared state.
        let landed = Instant::now();
        let fetched = snapshot(19.0);
        {
            let mut shared = lock_shared_budget();
            assert!(
                shared.claim_poll(Provider::Claude, false, landed),
                "nothing has polled yet in this process, so the claim must succeed"
            );
            shared.apply_read(Provider::Claude, ProviderRead::Ok(fetched.clone()), landed);
        }

        for (label, app) in [("first", &first), ("second", &second)] {
            app.update(cx, |app, cx| {
                app.sync_budget_from_shared(cx);
                assert_eq!(
                    app.budget
                        .get(Provider::Claude)
                        .last_ok
                        .clone()
                        .map(|(snapshot, _)| snapshot),
                    Some(fetched.clone()),
                    "the {label} window must render the numbers the single shared poll fetched, \
                     whichever window happened to fire it"
                );
            });
        }

        // And the guard the two windows share is the same one, so neither can start a second
        // read while the other's is open.
        assert!(
            !shared_budget()
                .lock()
                .expect("the shared budget is not poisoned")
                .claim_poll(Provider::Claude, false, landed),
            "a read that has just landed holds the whole process to POLL_INTERVAL, not one window"
        );
    }
}
