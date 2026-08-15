//! The budget model: which providers exist, which agent spends which one, what a window's
//! headroom means, and every string the pane strip and the popover print.
//!
//! Pure and GPUI-free, like `crate::status_bar::resources` - so "a connected provider that goes
//! stale shows `last read <age>` in place of its own numbers" is a property that can be tested
//! directly against a state value, without a window.
//!
//! # Headroom, not spend
//!
//! Both providers report a *utilisation* (percent **used**). Every number this module carries and
//! prints is the complement of that - percent **left** - which is what the popover's own footnote
//! (`counts headroom, not spend`) promises the reader. The conversion happens once, in
//! [`crate::budget::fetch`]'s parsers, so nothing downstream can hold a value whose direction is
//! ambiguous.

use std::time::{Duration, Instant, SystemTime};

use crate::work_surface::agents::{AgentKind, ProcessKind};

/// How often the background loop re-reads every connected provider
/// (`crate::budget::flow::AdeApp::start_budget_poll_loop`).
///
/// Deliberately slow. The windows themselves are 5 hours and 7 days long, so a percentage that
/// moves visibly inside five minutes does not exist; and the usage endpoints are themselves
/// rate-limited (a real, observed `429` with a `retry-after` header from Anthropic's own usage
/// endpoint while an ordinary authenticated `GET /api/oauth/profile` against the same token
/// returned `200` - the limiter is on the usage route specifically). A budget readout that spent
/// budget to fetch itself would be the worst possible failure mode here.
pub const POLL_INTERVAL: Duration = Duration::from_secs(300);

/// The floor between two *manual* reads of one provider (the popover's `Refresh`, and a failed
/// provider's `Retry`). A click inside this window is dropped rather than queued - see
/// [`ProviderBudget::may_poll_now`].
pub const MANUAL_REFRESH_FLOOR: Duration = Duration::from_secs(15);

/// How old a successful read may be before its numbers are replaced by `last read <age>`.
///
/// §2: "A **connected** provider that goes stale shows `last read <age>` in place of its own
/// numbers, so the signal attaches to what is broken." Staleness - not a failed attempt - is what
/// triggers that swap: a poll that failed seconds after a good read has numbers that are still
/// true, and hiding them would be less honest, not more. The failed attempt surfaces as the
/// `Retry` affordance instead ([`ProviderBudget::can_retry`]).
///
/// Three poll intervals: one missed tick is a hiccup, three in a row is a provider that has
/// stopped answering.
pub const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// Whether the background poll loop runs at all in this build.
///
/// **Off under `cfg(test)`, deliberately and non-negotiably.** The poll reads the *developer's
/// own* OAuth credential off disk and sends it to a real provider endpoint; a test suite that did
/// that would spend a real person's real rate-limit allowance (and hammer a limiter that is
/// already tight) every time anyone ran `cargo test`. Everything the loop does *around* the
/// network call - the single-flight guard, the manual-refresh floor, applying a result to state,
/// every derived readout - is tested directly against
/// `crate::budget::flow::AdeApp::apply_budget_poll_result`, and the two response parsers are
/// tested against real payloads. The one thing not covered by a test is "does the timer fire",
/// which is the same line the updater's own periodic loop draws.
pub const POLLING_ENABLED: bool = !cfg!(test);

/// A provider Jerry can read a rate-limit budget from.
///
/// Exactly the set this build can *spend*: `crate::work_surface::agents::AgentKind` has two
/// variants and each maps to one of these. This is not a wish list - a provider with no agent
/// kind has nothing in this app that could consume it, and a row for it in the popover would be
/// telemetry about a thing you cannot use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    /// Every provider, in the order `Other providers` lists them.
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    /// The lowercase name the pane strip prints (`Jerry.dc.html`'s `paneBudgetName` is
    /// `budgetDefs[].id`, lowercase).
    pub fn id(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    /// The capitalised name the popover's rows and lead header print
    /// (`budgetDefs[].label`).
    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }

    /// The two-letter chip in the popover's lead header (`budgetLeadChip`:
    /// `label.slice(0, 2).toLowerCase()`).
    pub fn chip(self) -> &'static str {
        match self {
            Provider::Claude => "cl",
            Provider::Codex => "co",
        }
    }
}

/// §4t's `provOf(agent)`: which provider an agent spends, or `None` for a pane that spends none.
///
/// A total function over [`ProcessKind`], not a lookup that can miss: the `match` is exhaustive,
/// so a third pane kind cannot be added without deciding what it spends. [`ProcessKind::Shell`] is
/// the `None` - §4t's "a local model shows nothing, correctly" lands in this build as "a shell
/// pane shows nothing", and for the same underlying reason: a surface that spends no provider has
/// no budget to attribute to it. That is a correct terminal state, not a gap waiting on a
/// provider-less agent kind.
pub fn provider_of(kind: ProcessKind) -> Option<Provider> {
    match kind {
        ProcessKind::Shell => None,
        ProcessKind::Agent(AgentKind::Claude) => Some(Provider::Claude),
        ProcessKind::Agent(AgentKind::Codex) => Some(Provider::Codex),
    }
}

/// The three-step hue rev 6 puts on a budget, on *remaining* budget rather than on consumption
/// (§2: "Hue `#7fc79a` above 40%, `#c99b4e` 15-40%, `#c4726d` below").
///
/// An enum rather than a colour directly, exactly like `crate::status_bar::resources::LoadLevel`:
/// the thresholds are then testable without a theme, and the render side reads the tokens back
/// (`crate::theme::budget::{OK, WARN, CRITICAL}`, which #279 already landed) rather than
/// inventing colour literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetLevel {
    /// Above 40% left - healthy. §2's rule for the whole attention family applies: "a budget only
    /// joins the attention family when it deserves to".
    Ok,
    /// 15-40% left.
    Warn,
    /// Below 15% left.
    Critical,
}

impl BudgetLevel {
    /// The real, resolved token for this step.
    pub fn color(self) -> crate::theme::ColorToken {
        use crate::theme::budget;
        match self {
            BudgetLevel::Ok => budget::OK,
            BudgetLevel::Warn => budget::WARN,
            BudgetLevel::Critical => budget::CRITICAL,
        }
    }
}

/// The step a real headroom percentage falls in. Boundaries follow §2's own wording exactly:
/// *above* 40 is healthy, 15-40 inclusive is amber, *below* 15 is red.
pub fn budget_level(headroom_percent: f32) -> BudgetLevel {
    if headroom_percent > 40.0 {
        BudgetLevel::Ok
    } else if headroom_percent >= 15.0 {
        BudgetLevel::Warn
    } else {
        BudgetLevel::Critical
    }
}

/// One rate-limit window of one provider: how much of it is left, when it resets, and what to
/// call it.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetWindow {
    /// The window's own duration, as the strip prints it - `5h`, `7d`. Owned rather than
    /// `&'static str` because it is **not** a constant on the Codex side: that API sends
    /// `limit_window_seconds` and the label is formatted from it ([`window_label`]), so a plan
    /// whose primary window is not five hours labels itself correctly instead of lying.
    pub label: String,
    /// Percent **left**, 0-100 - already the complement of the API's own "used" figure. See this
    /// module's docs.
    pub headroom_percent: f32,
    /// When this window rolls over, as an absolute instant, so the popover's countdown really
    /// counts down while it is open instead of freezing at whatever it read at poll time. `None`
    /// when the provider did not send one.
    pub resets_at: Option<SystemTime>,
}

impl BudgetWindow {
    pub fn level(&self) -> BudgetLevel {
        budget_level(self.headroom_percent)
    }

    /// `"92%"` - the value half of the readout. Rounded to whole percent: both APIs report whole
    /// or near-whole numbers, and a decimal place on a five-hour window is precision this fact
    /// does not have.
    pub fn value_label(&self) -> String {
        format!("{}%", self.headroom_percent.round() as i64)
    }

    /// `"92% left"` - the popover's own longer form, which has the room to remove the
    /// left/used ambiguity inline rather than only in the footnote.
    pub fn popover_value_label(&self) -> String {
        format!("{} left", self.value_label())
    }

    /// The meter's fill as a real 0.0-1.0 fraction.
    pub fn fill_fraction(&self) -> f32 {
        (self.headroom_percent / 100.0).clamp(0.0, 1.0)
    }

    /// `"resets in 4h 53m"`, or `None` when the provider sent no reset instant at all - which the
    /// render side draws as nothing rather than as a fabricated countdown.
    ///
    /// A window whose reset is already in the past reads `resets now`: the rollover is due and
    /// the next read will show it, which is a different (and more useful) statement than a
    /// negative duration or a stuck `0m`.
    pub fn reset_label(&self, now: SystemTime) -> Option<String> {
        let resets_at = self.resets_at?;
        let Ok(remaining) = resets_at.duration_since(now) else {
            return Some("resets now".to_string());
        };
        Some(format!("resets in {}", coarse_duration(remaining)))
    }
}

/// `"4h 53m"` / `"3d 6h"` / `"12m"` - two units at most, largest first, the shape §2's own
/// `resets 3d 6h` uses. Under a minute reads `<1m` rather than `0m`, which would look stuck.
pub fn coarse_duration(remaining: Duration) -> String {
    let total_minutes = remaining.as_secs() / 60;
    if total_minutes == 0 {
        return "<1m".to_string();
    }
    let days = total_minutes / (60 * 24);
    let hours = (total_minutes / 60) % 24;
    let minutes = total_minutes % 60;
    if days > 0 {
        return format!("{days}d {hours}h");
    }
    if hours > 0 {
        return format!("{hours}h {minutes}m");
    }
    format!("{minutes}m")
}

/// A window's own label from its real duration in seconds - `18000 -> "5h"`, `604800 -> "7d"`.
///
/// Exact units only: a window that is not a whole number of days or hours labels itself in the
/// next unit down (`90m`) rather than rounding, because the label's whole job is to say *which*
/// limit this bar is, and `2h` printed on a 150-minute window would name a window that does not
/// exist.
pub fn window_label(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let day = 60 * 60 * 24;
    if seconds >= day && seconds % day == 0 {
        return format!("{}d", seconds / day);
    }
    if seconds >= 3600 && seconds % 3600 == 0 {
        return format!("{}h", seconds / 3600);
    }
    if seconds >= 60 {
        return format!("{}m", seconds / 60);
    }
    format!("{seconds}s")
}

/// `"12s"` / `"3m"` / `"2h"` / `"4d"` - the compact age in `last read <age> ago`.
///
/// A second, shorter vocabulary than `crate::status_bar::resources::updated_ago_label`'s
/// spelled-out `"Updated 3 minutes ago"` on purpose, and only for this one string: it renders
/// *inside* the pane strip, in place of two meters and two percentages, where a spelled-out unit
/// would push the readout past the width it is replacing. The popover's own foot line - which has
/// the room - goes through the shared formatter, so the two surfaces still agree on the one fact
/// they both state.
pub fn compact_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

/// One provider's real, current windows - two of them for both providers this build supports, in
/// short-window-first order.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSnapshot {
    pub windows: Vec<BudgetWindow>,
}

impl ProviderSnapshot {
    /// The window with the least headroom - what ranks providers in the popover ("the tightest
    /// provider on top"). `None` for a provider that reported no windows at all, which is a real
    /// possibility for an account with no limits attached and is deliberately not coerced to
    /// `Some(100)`.
    pub fn tightest(&self) -> Option<&BudgetWindow> {
        self.windows.iter().min_by(|a, b| {
            a.headroom_percent
                .partial_cmp(&b.headroom_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// The `Other providers` row's one-line summary: `"5h 92%  ·  7d 65%"`.
    ///
    /// **Window label before the value**, like every other budget string in this app - §4c,
    /// verbatim: "`92% 5h` parsed as '92% for 5 hours'; `5h 92%` parses as '5-hour window, 92%
    /// left'".
    pub fn summary_label(&self) -> String {
        self.windows
            .iter()
            .map(|window| format!("{} {}", window.label, window.value_label()))
            .collect::<Vec<_>>()
            .join("  \u{b7}  ")
    }
}

/// What one provider's cluster - in the pane strip or in a popover row - should actually say
/// right now.
///
/// Five distinct states, not one nullable number. Rev 6 §7 rule 6, quoted in issue #294: "every
/// derived value must handle not-yet-polled, polled-ok, and poll-failed distinctly". `Checking`
/// and `NotConnected` are the two that a `Option<Snapshot>` would have collapsed into each other,
/// and they mean opposite things: one is "wait a moment", the other is "there is nothing here and
/// nothing is broken".
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderReadout<'a> {
    /// No credential for this provider on this machine. §4b's rule applies in the strip: a
    /// provider that is not connected never appears there at all.
    NotConnected,
    /// A credential exists but no poll has landed yet.
    Checking,
    /// A real, fresh read.
    Numbers(&'a ProviderSnapshot),
    /// Connected, but the last successful read is older than [`STALE_AFTER`] - §2's `last read
    /// <age>`, in place of the numbers themselves.
    LastRead(Duration),
    /// Connected, and no read has ever succeeded - so there is nothing to go stale, only a
    /// failure to report next to its `Retry`.
    RefreshFailed,
}

/// One provider's whole live state: whether it is connected at all, its most recent successful
/// read, whether the most recent *attempt* failed, and the poll bookkeeping.
///
/// Deliberately not an enum. The four facts are independent - a provider can hold good numbers
/// *and* a failed newest attempt at the same time, which is exactly the state §2's `last read
/// <age>` and §4c's `refresh failed · Retry` describe between them - and an enum would have to
/// duplicate the snapshot into several variants to say it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderBudget {
    /// Whether this provider's credential was found on disk at the last look. Re-checked on every
    /// poll, so logging into a provider's CLI while Jerry is running really does connect it.
    pub connected: bool,
    /// The most recent *successful* read, with the real instant it landed.
    pub last_ok: Option<(ProviderSnapshot, Instant)>,
    /// `Some` when the most recent attempt failed - the message is the popover's tooltip, and its
    /// presence is what earns the `Retry`. Cleared by a successful read.
    pub last_error: Option<String>,
    /// A poll is in flight for this provider right now. Single-flight per provider: a manual
    /// `Refresh` during the background poll's own request cannot start a second, racing one.
    pub in_flight: bool,
    /// The last time a poll was *started* for this provider, successful or not - the floor a
    /// manual refresh is measured against.
    pub last_attempt: Option<Instant>,
}

impl ProviderBudget {
    /// What this provider's cluster should print, given the clock.
    pub fn readout(&self, now: Instant) -> ProviderReadout<'_> {
        if !self.connected {
            return ProviderReadout::NotConnected;
        }
        match &self.last_ok {
            Some((snapshot, read_at)) => {
                let age = now.saturating_duration_since(*read_at);
                if age > STALE_AFTER {
                    ProviderReadout::LastRead(age)
                } else {
                    ProviderReadout::Numbers(snapshot)
                }
            }
            None if self.last_error.is_some() => ProviderReadout::RefreshFailed,
            None => ProviderReadout::Checking,
        }
    }

    /// Whether this provider's row earns a `Retry` - §4c puts one next to `refresh failed`, and
    /// the same affordance belongs to a provider whose numbers went stale *because* its polls
    /// started failing. A provider that is simply not connected has nothing to retry: Jerry does
    /// not log anyone in.
    pub fn can_retry(&self) -> bool {
        self.connected && self.last_error.is_some()
    }

    /// Whether a poll may start right now. `manual` clicks are additionally held to
    /// [`MANUAL_REFRESH_FLOOR`] since the last attempt; the background loop's own cadence is
    /// [`POLL_INTERVAL`], so it needs no second floor.
    pub fn may_poll_now(&self, manual: bool, now: Instant) -> bool {
        if self.in_flight {
            return false;
        }
        if !manual {
            return true;
        }
        match self.last_attempt {
            Some(at) => now.saturating_duration_since(at) >= MANUAL_REFRESH_FLOOR,
            None => true,
        }
    }

    /// How tight this provider is, for ranking - its own tightest window's headroom. `None` for
    /// anything with no usable numbers, which sorts after every provider that has some.
    pub fn tightest_headroom(&self, now: Instant) -> Option<f32> {
        match self.readout(now) {
            ProviderReadout::Numbers(snapshot) => {
                snapshot.tightest().map(|window| window.headroom_percent)
            }
            _ => None,
        }
    }
}

/// Every provider's state, keyed by provider - the whole feature's live data.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetState {
    claude: ProviderBudget,
    codex: ProviderBudget,
}

impl BudgetState {
    pub fn get(&self, provider: Provider) -> &ProviderBudget {
        match provider {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
        }
    }

    pub fn get_mut(&mut self, provider: Provider) -> &mut ProviderBudget {
        match provider {
            Provider::Claude => &mut self.claude,
            Provider::Codex => &mut self.codex,
        }
    }

    /// The most recent successful read across all providers - the popover's `Updated N ago` foot
    /// line. `None` until the very first read of any provider lands, which that line prints as an
    /// honest "not sampled yet" rather than as "just now".
    pub fn last_read_at(&self) -> Option<Instant> {
        Provider::ALL
            .iter()
            .filter_map(|provider| self.get(*provider).last_ok.as_ref().map(|(_, at)| *at))
            .max()
    }

    /// §4c's "the tightest provider at the top": the connected provider with the least headroom
    /// in any of its windows. Only a provider with real, fresh numbers can lead - a `not
    /// connected` or never-polled provider has no tightness to compare.
    pub fn lead_provider(&self, now: Instant) -> Option<Provider> {
        Provider::ALL
            .iter()
            .copied()
            .filter_map(|provider| {
                self.get(provider)
                    .tightest_headroom(now)
                    .map(|headroom| (provider, headroom))
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(provider, _)| provider)
    }
}

/// Real coverage for the model itself - the thresholds, the state machine, and every string.
#[cfg(test)]
mod budget_state_tests {
    use super::*;

    fn window(label: &str, headroom: f32) -> BudgetWindow {
        BudgetWindow {
            label: label.to_string(),
            headroom_percent: headroom,
            resets_at: None,
        }
    }

    fn snapshot(a: f32, b: f32) -> ProviderSnapshot {
        ProviderSnapshot {
            windows: vec![window("5h", a), window("7d", b)],
        }
    }

    /// §4t's mapping, including the case the issue is most explicit about: a pane that spends no
    /// provider shows nothing.
    #[test]
    fn every_pane_kind_maps_to_the_provider_it_actually_spends() {
        assert_eq!(
            provider_of(ProcessKind::claude()),
            Some(Provider::Claude),
            "a Claude agent spends Claude"
        );
        assert_eq!(
            provider_of(ProcessKind::codex()),
            Some(Provider::Codex),
            "a Codex agent spends Codex"
        );
        assert_eq!(
            provider_of(ProcessKind::Shell),
            None,
            "a shell pane spends no provider at all - \u{a7}4t's `a local model shows nothing, \
             correctly`, which in this build is the shell"
        );
    }

    /// §2's boundaries, at the exact numbers rather than near them - `40` is amber (only *above*
    /// 40 is healthy) and `15` is amber (only *below* 15 is red).
    #[test]
    fn the_hue_thresholds_sit_exactly_where_the_design_puts_them() {
        assert_eq!(budget_level(100.0), BudgetLevel::Ok);
        assert_eq!(budget_level(40.1), BudgetLevel::Ok);
        assert_eq!(
            budget_level(40.0),
            BudgetLevel::Warn,
            "\u{a7}2 says healthy is *above* 40%, so 40 itself is amber"
        );
        assert_eq!(budget_level(15.0), BudgetLevel::Warn);
        assert_eq!(
            budget_level(14.9),
            BudgetLevel::Critical,
            "\u{a7}2 says red is *below* 15%"
        );
        assert_eq!(budget_level(0.0), BudgetLevel::Critical);
    }

    /// The rendering decision issue #294's Phase 0 spike locked: two independent windows, hued
    /// **separately**. This is the exact live reading that settled it - a healthy 5h next to a 7d
    /// sitting on the amber boundary. One bar filled to the tighter window would paint the whole
    /// readout amber and say the session is constrained when it is not.
    #[test]
    fn each_window_takes_its_own_hue_rather_than_the_tighter_ones() {
        let snapshot = snapshot(81.0, 40.0);
        assert_eq!(snapshot.windows[0].level(), BudgetLevel::Ok);
        assert_eq!(snapshot.windows[1].level(), BudgetLevel::Warn);
        assert_eq!(
            snapshot.tightest().map(|w| w.label.clone()),
            Some("7d".to_string()),
            "the week is the tight one here - and a single bar would have shown only that"
        );
    }

    /// §4c, verbatim: "`92% 5h` parsed as '92% for 5 hours'; `5h 92%` parses as '5-hour window,
    /// 92% left'". The label leads.
    #[test]
    fn every_window_string_puts_the_window_label_before_the_value() {
        let snapshot = snapshot(92.0, 65.0);
        assert_eq!(snapshot.summary_label(), "5h 92%  \u{b7}  7d 65%");
        assert!(
            snapshot.summary_label().starts_with("5h "),
            "the window label must lead - `92% 5h` reads as `92% for 5 hours`"
        );
    }

    #[test]
    fn a_window_label_is_formatted_from_its_real_duration() {
        assert_eq!(window_label(5 * 3600), "5h");
        assert_eq!(window_label(7 * 86400), "7d");
        assert_eq!(window_label(86400), "1d");
        assert_eq!(
            window_label(150 * 60),
            "150m",
            "a window that is not a whole number of hours must not round to one that isn't real"
        );
        assert_eq!(window_label(30), "30s");
        assert_eq!(window_label(-5), "0s", "a negative duration is not a window");
    }

    #[test]
    fn a_reset_countdown_reads_largest_unit_first_and_never_goes_negative() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut window = window("5h", 92.0);

        window.resets_at = Some(now + Duration::from_secs(4 * 3600 + 53 * 60));
        assert_eq!(window.reset_label(now).as_deref(), Some("resets in 4h 53m"));

        window.resets_at = Some(now + Duration::from_secs(3 * 86400 + 6 * 3600));
        assert_eq!(window.reset_label(now).as_deref(), Some("resets in 3d 6h"));

        window.resets_at = Some(now + Duration::from_secs(30));
        assert_eq!(
            window.reset_label(now).as_deref(),
            Some("resets in <1m"),
            "under a minute must not read as a stuck `0m`"
        );

        window.resets_at = Some(now - Duration::from_secs(60));
        assert_eq!(
            window.reset_label(now).as_deref(),
            Some("resets now"),
            "a rollover that is already due is a real statement, not a negative duration"
        );

        window.resets_at = None;
        assert_eq!(
            window.reset_label(now),
            None,
            "a provider that sent no reset instant gets no fabricated countdown"
        );
    }

    /// The five states rev 6 §7 rule 6 insists stay distinct, walked in order.
    #[test]
    fn the_five_provider_states_are_all_genuinely_distinct() {
        let now = Instant::now();

        let mut budget = ProviderBudget::default();
        assert_eq!(
            budget.readout(now),
            ProviderReadout::NotConnected,
            "no credential on disk is `not connected`, and nothing about it is broken"
        );
        assert!(!budget.can_retry(), "there is nothing to retry when there is no credential");

        budget.connected = true;
        assert_eq!(
            budget.readout(now),
            ProviderReadout::Checking,
            "connected but never polled is its own state, not a fake 100%"
        );

        budget.last_error = Some("boom".to_string());
        assert_eq!(
            budget.readout(now),
            ProviderReadout::RefreshFailed,
            "a failure with no earlier numbers has nothing to go stale"
        );
        assert!(budget.can_retry(), "and it earns the `Retry` \u{a7}4c puts beside it");

        let fresh = snapshot(81.0, 40.0);
        budget.last_ok = Some((fresh.clone(), now));
        budget.last_error = None;
        assert_eq!(
            budget.readout(now),
            ProviderReadout::Numbers(&fresh),
            "a fresh read shows its real numbers"
        );

        // A failed attempt on top of *fresh* numbers keeps the numbers - they are still true -
        // and only adds the retry.
        budget.last_error = Some("boom".to_string());
        assert_eq!(
            budget.readout(now),
            ProviderReadout::Numbers(&fresh),
            "numbers seconds old are still true; the failed attempt shows up as the `Retry`, not \
             by hiding a fact we really have"
        );
        assert!(budget.can_retry());

        let stale_now = now + STALE_AFTER + Duration::from_secs(1);
        match budget.readout(stale_now) {
            ProviderReadout::LastRead(age) => assert!(
                age > STALE_AFTER,
                "\u{a7}2: a connected provider that goes stale shows `last read <age>` in place \
                 of its own numbers"
            ),
            other => panic!("expected `last read <age>`, got {other:?}"),
        }
    }

    #[test]
    fn a_manual_refresh_is_floored_but_the_background_poll_is_not() {
        let now = Instant::now();
        let mut budget = ProviderBudget {
            connected: true,
            ..Default::default()
        };

        assert!(
            budget.may_poll_now(true, now),
            "a provider that has never been polled may always be refreshed"
        );

        budget.last_attempt = Some(now);
        assert!(
            !budget.may_poll_now(true, now + Duration::from_secs(1)),
            "a second manual click a second later must be dropped, not queued - the endpoint \
             behind it is itself rate-limited"
        );
        assert!(
            budget.may_poll_now(true, now + MANUAL_REFRESH_FLOOR),
            "and allowed again once the floor has passed"
        );
        assert!(
            budget.may_poll_now(false, now + Duration::from_secs(1)),
            "the background loop has its own cadence and is not held to the manual floor"
        );

        budget.in_flight = true;
        assert!(
            !budget.may_poll_now(false, now + POLL_INTERVAL),
            "single-flight per provider: nothing starts a second request while one is open"
        );
    }

    /// §4c: "the tightest provider at the top".
    #[test]
    fn the_lead_provider_is_the_tightest_one_with_real_numbers() {
        let now = Instant::now();
        let mut state = BudgetState::default();
        assert_eq!(
            state.lead_provider(now),
            None,
            "nothing connected means nothing leads"
        );

        state.get_mut(Provider::Claude).connected = true;
        state.get_mut(Provider::Claude).last_ok = Some((snapshot(81.0, 40.0), now));
        assert_eq!(state.lead_provider(now), Some(Provider::Claude));

        state.get_mut(Provider::Codex).connected = true;
        state.get_mut(Provider::Codex).last_ok = Some((snapshot(99.0, 12.0), now));
        assert_eq!(
            state.lead_provider(now),
            Some(Provider::Codex),
            "codex's 12% weekly is tighter than claude's 40%, even though its session window is \
             the healthier of the two"
        );

        // A provider whose numbers went stale cannot lead on numbers nobody can see.
        let stale_now = now + STALE_AFTER + Duration::from_secs(1);
        assert_eq!(state.lead_provider(stale_now), None);
    }

    #[test]
    fn the_compact_age_steps_through_every_unit() {
        assert_eq!(compact_age(Duration::from_secs(12)), "12s");
        assert_eq!(compact_age(Duration::from_secs(180)), "3m");
        assert_eq!(compact_age(Duration::from_secs(7200)), "2h");
        assert_eq!(compact_age(Duration::from_secs(4 * 86400)), "4d");
    }

    #[test]
    fn the_last_read_instant_is_the_newest_across_providers() {
        let now = Instant::now();
        let mut state = BudgetState::default();
        assert_eq!(state.last_read_at(), None, "not sampled yet is its own state");

        let older = now - Duration::from_secs(120);
        state.get_mut(Provider::Claude).last_ok = Some((snapshot(50.0, 50.0), older));
        state.get_mut(Provider::Codex).last_ok = Some((snapshot(50.0, 50.0), now));
        assert_eq!(state.last_read_at(), Some(now));
    }
}
