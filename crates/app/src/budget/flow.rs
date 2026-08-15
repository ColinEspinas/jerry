//! The poll: when Jerry reads a provider's budget, what stops it reading too often, and how a
//! real result becomes state.
//!
//! Every network call here runs on `cx.background_executor()` and mutates [`AdeApp`] only
//! afterwards, from inside the `this.update(...)` closure - the same discipline
//! `crate::review::flow` and `crate::updater::flow` already follow.

use std::time::Instant;

use super::fetch::{self, ProviderRead};
use super::state::{Provider, POLLING_ENABLED, POLL_INTERVAL};
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
    /// the update-check loop it is modelled on.
    ///
    /// Does nothing at all under `cfg(test)` ([`POLLING_ENABLED`]); see that constant's docs for
    /// why a test suite must never spend a real person's rate-limit allowance.
    pub(crate) fn start_budget_poll_loop(&mut self, cx: &mut Context<Self>) {
        if !POLLING_ENABLED {
            return;
        }
        self.poll_budgets_if_due(cx);
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(POLL_HEARTBEAT).await;
            let alive = this.update(cx, |this, cx| {
                this.poll_budgets_if_due(cx);
            });
            if alive.is_err() {
                break;
            }
        });
        self._budget_poll_task = Some(task);
    }

    /// One heartbeat: read every provider whose turn it is.
    ///
    /// **Nothing is polled while no agent session is open at all.** The readout lives in an agent
    /// pane and the popover is reachable only from one (§4u′'s accepted trade-off), so a window
    /// showing only shell tabs has nowhere to display a budget - and a background HTTP request
    /// whose result cannot be seen is exactly the "telemetry about telemetry" §2 rejects. It also
    /// means the common case of Jerry sitting open on a terminal costs a provider nothing.
    pub(crate) fn poll_budgets_if_due(&mut self, cx: &mut Context<Self>) {
        if !self
            .agents
            .iter()
            .any(|agent| agent.kind.is_agent_session())
        {
            return;
        }
        let now = Instant::now();
        for provider in Provider::ALL {
            let budget = self.budget.get(provider);
            let due = match budget.last_attempt {
                Some(at) => now.saturating_duration_since(at) >= POLL_INTERVAL,
                None => true,
            };
            if due {
                self.refresh_provider_budget(provider, false, cx);
            }
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
    /// # The `cfg(test)` guard lives here, not only on the timer
    ///
    /// This is the single choke point every read passes through: the background loop, the
    /// popover's `Refresh` and a failed row's `Retry` all arrive here, and only the first of the
    /// three goes anywhere near [`AdeApp::start_budget_poll_loop`]'s own [`POLLING_ENABLED`]
    /// check. A test that drove either control - a click test on the popover, a render test that
    /// happened to fire the handler - would otherwise read the developer's own OAuth credential
    /// off disk and send it to a real provider, which is precisely what that constant exists to
    /// prevent. Gating the loop alone made the promise true by accident of what the suite happens
    /// to call today; gating here makes it true by construction.
    pub(crate) fn refresh_provider_budget(
        &mut self,
        provider: Provider,
        manual: bool,
        cx: &mut Context<Self>,
    ) {
        if !POLLING_ENABLED {
            return;
        }
        let now = Instant::now();
        if !self.budget.get(provider).may_poll_now(manual, now) {
            return;
        }
        {
            let budget = self.budget.get_mut(provider);
            budget.in_flight = true;
            budget.last_attempt = Some(now);
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { fetch::read_provider(provider) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_budget_poll_result(provider, read, Instant::now());
                cx.notify();
            });
        })
        .detach();
    }

    /// Folds one real read into one provider's state. The whole state machine, in one place and
    /// with no `Context` - so every transition is directly testable without a window or a network.
    ///
    /// The three outcomes are kept genuinely distinct (rev 6 §7 rule 6):
    ///
    /// - **not connected** clears everything, including any numbers from before. A provider whose
    ///   credential has gone (a logout while Jerry was running) is not a provider with stale
    ///   numbers - the data is not ours to show any more.
    /// - **ok** replaces the numbers and clears the failure.
    /// - **failed** records the reason and *keeps* the previous numbers. They are still the last
    ///   true reading; §2's `last read <age>` takes over once they age past
    ///   [`super::state::STALE_AFTER`], and the failure itself surfaces as the `Retry`.
    pub(crate) fn apply_budget_poll_result(
        &mut self,
        provider: Provider,
        read: ProviderRead,
        now: Instant,
    ) {
        let budget = self.budget.get_mut(provider);
        budget.in_flight = false;
        match read {
            ProviderRead::NotConnected => {
                budget.connected = false;
                budget.last_ok = None;
                budget.last_error = None;
            }
            ProviderRead::Ok(snapshot) => {
                budget.connected = true;
                budget.last_ok = Some((snapshot, now));
                budget.last_error = None;
            }
            ProviderRead::Failed(reason) => {
                budget.connected = true;
                log::warn!("{} rate-limit read failed: {reason}", provider.id());
                budget.last_error = Some(reason);
            }
        }
    }
}

/// Real coverage for the state machine the poll drives, without touching the network.
#[cfg(test)]
mod budget_flow_tests {
    use super::super::state::{ProviderReadout, ProviderSnapshot, STALE_AFTER};
    use super::*;
    use crate::budget::state::BudgetWindow;
    use crate::root::focus::palette_focus_tests::open_test_app;
    use gpui::TestAppContext;

    fn snapshot(headroom: f32) -> ProviderSnapshot {
        ProviderSnapshot {
            windows: vec![BudgetWindow {
                label: "5h".to_string(),
                headroom_percent: headroom,
                resets_at: None,
            }],
        }
    }

    /// The poll must never run itself in a test build - it reads a real credential off the
    /// developer's disk and sends it to a real provider. Asserted at *compile* time rather than in
    /// a `#[test]` body: a run-time assertion can only fail after the suite has already had the
    /// chance to make the call it is guarding against, whereas this one refuses to build a test
    /// binary in which polling is on.
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

    #[gpui::test]
    fn a_real_read_becomes_state_and_a_failure_keeps_the_numbers_it_had(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            let now = Instant::now();

            app.apply_budget_poll_result(Provider::Claude, ProviderRead::NotConnected, now);
            assert_eq!(
                app.budget.get(Provider::Claude).readout(now),
                ProviderReadout::NotConnected,
                "no credential is `not connected`, and nothing was sent anywhere"
            );

            let good = snapshot(81.0);
            app.apply_budget_poll_result(Provider::Claude, ProviderRead::Ok(good.clone()), now);
            assert_eq!(
                app.budget.get(Provider::Claude).readout(now),
                ProviderReadout::Numbers(&good)
            );
            assert!(
                !app.budget.get(Provider::Claude).in_flight,
                "a landed result always clears the single-flight guard"
            );

            app.apply_budget_poll_result(
                Provider::Claude,
                ProviderRead::Failed("the provider answered 429".to_string()),
                now,
            );
            assert_eq!(
                app.budget.get(Provider::Claude).readout(now),
                ProviderReadout::Numbers(&good),
                "a failed refresh does not erase numbers that are still true"
            );
            assert!(
                app.budget.get(Provider::Claude).can_retry(),
                "but it does earn the `Retry` \u{a7}4c puts beside a failure"
            );

            let stale = now + STALE_AFTER + std::time::Duration::from_secs(1);
            assert!(
                matches!(
                    app.budget.get(Provider::Claude).readout(stale),
                    ProviderReadout::LastRead(_)
                ),
                "\u{a7}2: once they go stale the numbers give way to `last read <age>`"
            );

            // A logout mid-session: the credential is gone, and so are the numbers.
            app.apply_budget_poll_result(Provider::Claude, ProviderRead::NotConnected, now);
            assert_eq!(
                app.budget.get(Provider::Claude).readout(now),
                ProviderReadout::NotConnected
            );
            assert!(
                app.budget.get(Provider::Claude).last_ok.is_none(),
                "numbers from a provider we are no longer logged into are not ours to show"
            );
        });
    }

    /// The heartbeat must not touch the network for a window that has no agent at all - there is
    /// nowhere to render a budget, and a request whose result cannot be seen is exactly the
    /// telemetry §2 rejects.
    #[gpui::test]
    fn a_window_with_no_agent_session_polls_nothing(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            assert!(
                !app.agents.iter().any(|agent| agent.kind.is_agent_session()),
                "premise: the startup pane is a shell, not an agent session"
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
}
