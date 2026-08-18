//! The budget model: which providers exist, which agent spends which one, what a window's
//! usage means, and every string the pane strip and the popover print.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::fetch::ProviderRead;
use crate::work_surface::agents::{AgentKind, ProcessKind};

/// How often the background loop re-reads every connected provider
/// (`crate::budget::flow::AdeApp::start_budget_poll_loop`).
pub const POLL_INTERVAL: Duration = Duration::from_secs(300);

/// The floor between two *manual* reads of one provider (the popover's `Refresh`, and a failed
/// provider's `Retry`). A click inside this window is dropped rather than queued - see
/// [`ProviderBudget::may_poll_now`].
pub const MANUAL_REFRESH_FLOOR: Duration = Duration::from_secs(15);

/// How old a successful read may be before its numbers are replaced by `last read <age>`.
pub const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// How long [`ProviderBudget::in_flight`] may stay set before it is treated as a lost poll rather
/// than as a running one.
pub const IN_FLIGHT_STALE_AFTER: Duration = Duration::from_secs(60);

/// Whether this *build* compiles the provider poll in at all - `false` under `cfg(test)`.
pub const POLLING_ENABLED: bool = !cfg!(test);

/// Set this to anything (`JERRY_DISABLE_PROVIDER_POLL=1`) and no provider is ever read, in any
/// build.
pub const DISABLE_PROVIDER_POLL_ENV: &str = "JERRY_DISABLE_PROVIDER_POLL";

/// Whether a real provider read may happen in this process, right now. The single question every
/// caller in `crate::budget::flow` asks before touching a credential or the network.
pub fn polling_enabled() -> bool {
    polling_enabled_from(POLLING_ENABLED, std::env::var_os(DISABLE_PROVIDER_POLL_ENV))
}

/// The pure half of [`polling_enabled`] - so the rule is tested without mutating the
/// process-global environment (`std::env::set_var` is unsound to race in a threaded test binary),
/// the same split [`crate::budget::fetch::credential_dir_from`] already uses for its own
/// environment override.
pub fn polling_enabled_from(compiled_in: bool, disable_env: Option<std::ffi::OsString>) -> bool {
    compiled_in && disable_env.is_none()
}

/// A provider Jerry can read a rate-limit budget from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    /// Every provider, in the order `Other providers` lists them.
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    /// The lowercase name the pane strip prints.
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
pub fn provider_of(kind: ProcessKind) -> Option<Provider> {
    match kind {
        ProcessKind::Shell => None,
        ProcessKind::Agent(AgentKind::Claude) => Some(Provider::Claude),
        ProcessKind::Agent(AgentKind::Codex) => Some(Provider::Codex),
        // Cursor spends a real budget, but nothing here can read it: the popover's numbers come
        // from `crate::budget::fetch`'s real Claude/Codex endpoints, and there is no Cursor
        // equivalent wired up. `None` is the honest answer - a third row here would be a
        // fabricated one.
        ProcessKind::Agent(AgentKind::Cursor) => None,
    }
}

/// The three-step hue rev 6 puts on a budget, on *remaining* budget rather than on consumption
/// (§2: "Hue `#7fc79a` above 40%, `#c99b4e` 15-40%, `#c4726d` below").
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

/// One rate-limit window of one provider: how much of it has been spent, when it resets, and what
/// to call it.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetWindow {
    /// The window's own duration, as the strip prints it - `5h`, `7d`. Owned rather than
    /// `&'static str` because it is **not** a constant on the Codex side: that API sends
    /// `limit_window_seconds` and the label is formatted from it ([`window_label`]), so a plan
    /// whose primary window is not five hours labels itself correctly instead of lying.
    pub label: String,
    /// Percent **used**, 0-100 - exactly the figure both providers report (Claude's
    /// `utilization`, Codex's `used_percent`), clamped but not converted. See this module's docs
    /// for why this is stored the way the API sends it rather than as its complement.
    pub used_percent: f32,
    /// When this window rolls over, as an absolute instant, so the popover's countdown really
    /// counts down while it is open instead of freezing at whatever it read at poll time. `None`
    /// when the provider did not send one.
    pub resets_at: Option<SystemTime>,
}

impl BudgetWindow {
    /// Percent **left** - the complement of [`Self::used_percent`], and the only thing that reads
    /// it. Nothing on screen prints this; it exists because the hue thresholds are defined on
    /// remaining budget ([`budget_level`]) and must stay that way whatever the printed number
    /// says.
    pub fn headroom_percent(&self) -> f32 {
        (100.0 - self.used_percent).clamp(0.0, 100.0)
    }

    pub fn level(&self) -> BudgetLevel {
        budget_level(self.headroom_percent())
    }

    /// `"61%"` - the value half of the readout, as percent **used**. Rounded to whole percent:
    /// both APIs report whole or near-whole numbers, and a decimal place on a five-hour window is
    /// precision this fact does not have.
    pub fn value_label(&self) -> String {
        format!("{}%", self.used_percent.round() as i64)
    }

    /// `"61% used"` - the popover's own longer form, which has the room to say which direction
    /// the number runs in inline rather than only in the footnote.
    pub fn popover_value_label(&self) -> String {
        format!("{} used", self.value_label())
    }

    /// The meter's fill as a real 0.0-1.0 fraction: the **usage**, so the bar fills up as the
    /// window is spent and a nearly-full red bar sits beside a high `% used`. See this module's
    /// docs - the bar and the number have to tell one story, and this is which one.
    pub fn fill_fraction(&self) -> f32 {
        (self.used_percent / 100.0).clamp(0.0, 1.0)
    }

    /// `"resets in 4h 53m"`, or `None` when the provider sent no reset instant at all - which the
    /// render side draws as nothing rather than as a fabricated countdown.
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
    /// The tightest window - the most spent, equivalently the one with the least headroom - which
    /// is what ranks providers in the popover ("the tightest provider on top"). `None` for a
    /// provider that reported no windows at all, which is a real possibility for an account with
    /// no limits attached and is deliberately not coerced to `Some(0)`.
    pub fn tightest(&self) -> Option<&BudgetWindow> {
        self.windows.iter().min_by(|a, b| {
            a.headroom_percent()
                .partial_cmp(&b.headroom_percent())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// The `Other providers` row's one-line summary: `"5h 8%  ·  7d 35%"`.
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderBudget {
    /// Whether this provider's credential was found on disk at the last look. Re-checked on every
    /// poll, so logging into a provider's CLI while Jerry is running really does connect it.
    pub connected: bool,
    /// The most recent *successful* read, with the real instant it landed.
    pub last_ok: Option<(ProviderSnapshot, Instant)>,
    /// `Some` when the most recent attempt failed - the message is the tooltip on that provider's
    /// own popover row (`crate::budget::render::AdeApp::render_budget_other_row`), and its
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

    /// Whether a poll may start right now: nothing already open, and long enough since the last
    /// attempt - [`MANUAL_REFRESH_FLOOR`] for a click, the full [`POLL_INTERVAL`] for the
    /// background loop.
    pub fn may_poll_now(&self, manual: bool, now: Instant) -> bool {
        if self.in_flight && !self.in_flight_is_stale(now) {
            return false;
        }
        let floor = if manual {
            MANUAL_REFRESH_FLOOR
        } else {
            POLL_INTERVAL
        };
        match self.last_attempt {
            Some(at) => now.saturating_duration_since(at) >= floor,
            None => true,
        }
    }

    /// Whether an open request has been open so long that it is better explained as lost than as
    /// running. A guard with no `last_attempt` behind it cannot be aged (nothing says when it was
    /// set) and is left alone - that combination is not reachable, because the two are written
    /// together in [`BudgetState::claim_poll`].
    fn in_flight_is_stale(&self, now: Instant) -> bool {
        match self.last_attempt {
            Some(at) => now.saturating_duration_since(at) >= IN_FLIGHT_STALE_AFTER,
            None => false,
        }
    }

    /// How tight this provider is, for ranking - its own tightest window's headroom. `None` for
    /// anything with no usable numbers, which sorts after every provider that has some.
    pub fn tightest_headroom(&self, now: Instant) -> Option<f32> {
        match self.readout(now) {
            ProviderReadout::Numbers(snapshot) => {
                snapshot.tightest().map(|window| window.headroom_percent())
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
    /// Takes the right to start exactly one real read of `provider`, marking it in flight, or
    /// answers `false` because something else already holds that right (or because a click came
    /// inside [`MANUAL_REFRESH_FLOOR`]).
    pub fn claim_poll(&mut self, provider: Provider, manual: bool, now: Instant) -> bool {
        if !self.get(provider).may_poll_now(manual, now) {
            return false;
        }
        let budget = self.get_mut(provider);
        budget.in_flight = true;
        budget.last_attempt = Some(now);
        true
    }

    /// Folds one real read into one provider's state. The whole state machine, in one place and
    /// with no `Context` and no window - so every transition is directly testable without either.
    pub fn apply_read(&mut self, provider: Provider, read: ProviderRead, now: Instant) {
        let budget = self.get_mut(provider);
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

/// The one budget this **process** has, shared by every window in it.
pub fn shared_budget() -> &'static Mutex<BudgetState> {
    static SHARED: OnceLock<Mutex<BudgetState>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(BudgetState::default()))
}

/// [`shared_budget`], locked, recovering rather than propagating if a previous holder panicked.
pub fn lock_shared_budget() -> MutexGuard<'static, BudgetState> {
    shared_budget()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Real coverage for the model itself - the thresholds, the state machine, and every string.
#[cfg(test)]
mod budget_state_tests {
    use super::*;

    /// A window of `used` percent - the direction every one of these numbers runs in, so a test
    /// that reads `19.0` means "19% spent, 81% left" and never the other way round.
    fn window(label: &str, used: f32) -> BudgetWindow {
        BudgetWindow {
            label: label.to_string(),
            used_percent: used,
            resets_at: None,
        }
    }

    fn snapshot(a: f32, b: f32) -> ProviderSnapshot {
        ProviderSnapshot {
            windows: vec![window("5h", a), window("7d", b)],
        }
    }

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

    #[test]
    fn each_window_takes_its_own_hue_rather_than_the_tighter_ones() {
        let snapshot = snapshot(19.0, 60.0);
        assert_eq!(snapshot.windows[0].level(), BudgetLevel::Ok);
        assert_eq!(snapshot.windows[1].level(), BudgetLevel::Warn);
        assert_eq!(
            snapshot.tightest().map(|w| w.label.clone()),
            Some("7d".to_string()),
            "the week is the tight one here - and a single bar would have shown only that"
        );
    }

    #[test]
    fn every_window_string_puts_the_window_label_before_the_value() {
        let snapshot = snapshot(8.0, 35.0);
        assert_eq!(snapshot.summary_label(), "5h 8%  \u{b7}  7d 35%");
        assert!(
            snapshot.summary_label().starts_with("5h "),
            "the window label must lead - `8% 5h` reads as `8% for 5 hours`"
        );
    }

    #[test]
    fn every_printed_percentage_is_usage_and_the_meter_fills_with_it() {
        let nearly_spent = window("5h", 95.0);
        assert_eq!(
            nearly_spent.value_label(),
            "95%",
            "the strip prints what has been spent, not what is left"
        );
        assert_eq!(nearly_spent.popover_value_label(), "95% used");
        assert!(
            (nearly_spent.fill_fraction() - 0.95).abs() < f32::EPSILON,
            "and the meter is nearly full, because the bar and the number have to tell one story"
        );
        assert_eq!(
            nearly_spent.level(),
            BudgetLevel::Critical,
            "95% used is 5% left, which is red - the hue keys off headroom however the number is \
             printed, and inverting it along with the display would paint a spent window green"
        );

        let untouched = window("7d", 0.0);
        assert_eq!(untouched.value_label(), "0%");
        assert_eq!(untouched.fill_fraction(), 0.0, "an unspent window is empty");
        assert_eq!(untouched.level(), BudgetLevel::Ok);

        // An over-quota account can report past 100; the meter must not overflow its own track,
        // and the number must not read as a percentage that cannot exist.
        let over_quota = window("5h", 130.0);
        assert_eq!(over_quota.fill_fraction(), 1.0);
        assert_eq!(over_quota.headroom_percent(), 0.0);
        assert_eq!(over_quota.level(), BudgetLevel::Critical);
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
        assert_eq!(
            window_label(-5),
            "0s",
            "a negative duration is not a window"
        );
    }

    #[test]
    fn a_reset_countdown_reads_largest_unit_first_and_never_goes_negative() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut window = window("5h", 8.0);

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

    #[test]
    fn the_five_provider_states_are_all_genuinely_distinct() {
        let now = Instant::now();

        let mut budget = ProviderBudget::default();
        assert_eq!(
            budget.readout(now),
            ProviderReadout::NotConnected,
            "no credential on disk is `not connected`, and nothing about it is broken"
        );
        assert!(
            !budget.can_retry(),
            "there is nothing to retry when there is no credential"
        );

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
        assert!(
            budget.can_retry(),
            "and it earns the `Retry` \u{a7}4c puts beside it"
        );

        let fresh = snapshot(19.0, 60.0);
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
    fn a_manual_refresh_is_floored_and_the_background_poll_holds_the_full_interval() {
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
            !budget.may_poll_now(false, now + MANUAL_REFRESH_FLOOR),
            "the background loop is held to its own, much longer cadence - a click may jump the \
             queue, a timer may not"
        );
        assert!(
            budget.may_poll_now(false, now + POLL_INTERVAL),
            "and takes its turn once a full interval has passed"
        );

        budget.in_flight = true;
        assert!(
            !budget.may_poll_now(false, now + IN_FLIGHT_STALE_AFTER / 2),
            "single-flight per provider: nothing starts a second request while one is open"
        );
        assert!(
            budget.may_poll_now(false, now + POLL_INTERVAL),
            "but the guard is not trusted past the point where the request must be lost - see \
             `a_lost_poll_ages_out_instead_of_wedging_the_provider_forever`"
        );
    }

    #[test]
    fn a_lost_poll_ages_out_instead_of_wedging_the_provider_forever() {
        let started = Instant::now();
        let budget = ProviderBudget {
            connected: true,
            in_flight: true,
            last_attempt: Some(started),
            ..Default::default()
        };

        assert!(
            !budget.may_poll_now(false, started + Duration::from_secs(1)),
            "single-flight still holds while the request is plausibly still running"
        );
        assert!(
            !budget.may_poll_now(
                true,
                started + IN_FLIGHT_STALE_AFTER - Duration::from_secs(1)
            ),
            "and right up to the age-out, for a click as much as for the loop"
        );
        assert!(
            budget.may_poll_now(true, started + IN_FLIGHT_STALE_AFTER),
            "past it the request is better explained as lost than as running: the `Retry` button \
             must come back to life rather than stay dead for the session, since it is the \
             control a user reaches for when the readout looks stuck"
        );
        assert!(
            budget.may_poll_now(false, started + POLL_INTERVAL),
            "and the background loop takes the provider back over at its own next turn - the \
             age-out lifts the guard, it does not shorten the cadence"
        );
    }

    #[test]
    fn every_landed_result_clears_the_single_flight_guard() {
        let now = Instant::now();
        for read in [
            ProviderRead::NotConnected,
            ProviderRead::Ok(snapshot(19.0, 60.0)),
            ProviderRead::Failed("the provider answered 429".to_string()),
        ] {
            let mut state = BudgetState::default();
            assert!(
                state.claim_poll(Provider::Claude, false, now),
                "a fresh provider may always be claimed"
            );
            assert!(state.get(Provider::Claude).in_flight);
            state.apply_read(Provider::Claude, read.clone(), now);
            assert!(
                !state.get(Provider::Claude).in_flight,
                "{read:?} must clear the guard - a result that lands and leaves it set is the \
                 wedge in another costume"
            );
        }
    }

    #[test]
    fn a_real_read_becomes_state_and_a_failure_keeps_the_numbers_it_had() {
        let now = Instant::now();
        let mut state = BudgetState::default();

        state.apply_read(Provider::Claude, ProviderRead::NotConnected, now);
        assert_eq!(
            state.get(Provider::Claude).readout(now),
            ProviderReadout::NotConnected,
            "no credential is `not connected`, and nothing was sent anywhere"
        );

        let good = snapshot(19.0, 60.0);
        state.apply_read(Provider::Claude, ProviderRead::Ok(good.clone()), now);
        assert_eq!(
            state.get(Provider::Claude).readout(now),
            ProviderReadout::Numbers(&good)
        );

        state.apply_read(
            Provider::Claude,
            ProviderRead::Failed("the provider answered 429".to_string()),
            now,
        );
        assert_eq!(
            state.get(Provider::Claude).readout(now),
            ProviderReadout::Numbers(&good),
            "a failed refresh does not erase numbers that are still true"
        );
        assert!(
            state.get(Provider::Claude).can_retry(),
            "but it does earn the `Retry` \u{a7}4c puts beside a failure"
        );
        assert_eq!(
            state.get(Provider::Claude).last_error.as_deref(),
            Some("the provider answered 429"),
            "and the real reason is kept, for the row's tooltip"
        );

        state.apply_read(Provider::Claude, ProviderRead::NotConnected, now);
        assert_eq!(
            state.get(Provider::Claude).readout(now),
            ProviderReadout::NotConnected
        );
        assert!(
            state.get(Provider::Claude).last_ok.is_none(),
            "numbers from a provider we are no longer logged into are not ours to show"
        );
    }

    #[test]
    fn two_windows_claiming_the_same_budget_get_one_poll_between_them() {
        let now = Instant::now();
        let mut shared = BudgetState::default();

        assert!(
            shared.claim_poll(Provider::Claude, false, now),
            "the first window's heartbeat starts the read"
        );
        assert!(
            !shared.claim_poll(Provider::Claude, false, now),
            "the second window's heartbeat, a moment later, must find the read already open - \
             not open a competing one against an endpoint that answers 429"
        );
        assert!(
            !shared.claim_poll(Provider::Claude, true, now),
            "and a `Refresh` clicked in that second window is held to the same shared guard"
        );

        shared.apply_read(
            Provider::Claude,
            ProviderRead::Ok(snapshot(19.0, 60.0)),
            now,
        );
        assert!(
            !shared.claim_poll(Provider::Claude, false, now + POLL_INTERVAL / 2),
            "the cadence is shared too: half an interval later neither window may poll"
        );
        assert!(
            shared.claim_poll(Provider::Claude, false, now + POLL_INTERVAL),
            "and whichever window's heartbeat gets there first takes the next one"
        );
    }

    #[test]
    fn the_shared_budget_is_a_single_process_wide_value() {
        assert!(
            std::ptr::eq(shared_budget(), shared_budget()),
            "every window must claim against the same state, or none of the rate-limit rules \
             mean anything across windows"
        );
    }

    #[test]
    fn the_environment_kill_switch_disables_polling_in_any_build() {
        assert!(
            polling_enabled_from(true, None),
            "a real build with no switch set polls, or the feature does not exist"
        );
        assert!(
            !polling_enabled_from(true, Some(std::ffi::OsString::from("1"))),
            "and the switch turns it off even in a build that compiled it in - the case an \
             integration-test crate would otherwise walk straight into, reading the developer's \
             own OAuth credential and spending their real allowance"
        );
        assert!(
            !polling_enabled_from(true, Some(std::ffi::OsString::from(""))),
            "set-but-empty is still set: a harness that exports it without a value means it"
        );
        assert!(
            !polling_enabled_from(false, None),
            "and this crate's own test targets stay off with or without it"
        );
    }

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
        state.get_mut(Provider::Claude).last_ok = Some((snapshot(19.0, 60.0), now));
        assert_eq!(state.lead_provider(now), Some(Provider::Claude));

        state.get_mut(Provider::Codex).connected = true;
        state.get_mut(Provider::Codex).last_ok = Some((snapshot(1.0, 88.0), now));
        assert_eq!(
            state.lead_provider(now),
            Some(Provider::Codex),
            "codex's 88%-spent week is tighter than claude's 60%, even though its session window \
             is the healthier of the two"
        );

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
        assert_eq!(
            state.last_read_at(),
            None,
            "not sampled yet is its own state"
        );

        let older = now - Duration::from_secs(120);
        state.get_mut(Provider::Claude).last_ok = Some((snapshot(50.0, 50.0), older));
        state.get_mut(Provider::Codex).last_ok = Some((snapshot(50.0, 50.0), now));
        assert_eq!(state.last_read_at(), Some(now));
    }
}
