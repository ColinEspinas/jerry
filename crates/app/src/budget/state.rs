//! The budget model: which providers exist, which agent spends which one, what a window's
//! usage means, and every string the pane strip and the popover print.
//!
//! Pure and GPUI-free, like `crate::status_bar::resources` - so "a connected provider that goes
//! stale shows `last read <age>` in place of its own numbers" is a property that can be tested
//! directly against a state value, without a window.
//!
//! # Used, not left
//!
//! Both providers report a *utilisation* (percent **used**), and that is the number this module
//! carries and prints, unconverted: `61% used`, on a meter that fills as it is spent. The design
//! bundle specified the complement (`65% left`, on a meter where full is good), and this build
//! shipped that first; it was overruled by the product owner after seeing it, and the reason is
//! worth recording, because it is the sort of thing that gets "corrected" back by whoever next
//! reads the bundle. `% used` is the figure both providers' own APIs report, the figure both
//! CLIs' own `/status` displays show, and the one a reader already has in their head from every
//! other quota they have ever seen - and a meter that empties as you work reads as a *drain* even
//! when the number beside it is fine.
//!
//! Two consequences worth stating plainly, because getting either backwards would be a real bug
//! rather than a matter of taste:
//!
//! - **The bar and the number tell the same story.** [`BudgetWindow::fill_fraction`] is the
//!   *usage*, so a nearly-full bar sits beside a high `% used` and both mean "nearly spent".
//! - **The hue still keys off what is left.** [`budget_level`] takes *headroom*
//!   ([`BudgetWindow::headroom_percent`]), because "amber below 40% left" is a statement about
//!   remaining budget and stays true however the number is printed. `95% used` is red, not green.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::fetch::ProviderRead;
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

/// How long [`ProviderBudget::in_flight`] may stay set before it is treated as a lost poll rather
/// than as a running one.
///
/// The guard is cleared by a landing result, and [`crate::budget::fetch::read_provider_catching_panics`]
/// plus `crate::budget::flow`'s own "apply to the shared state before touching the window"
/// ordering are what make a result land on every ordinary path, panic included. This constant is
/// what covers the paths *nobody* can enumerate - an executor thread that dies, a future that is
/// dropped mid-await, a bug not yet written. Without it a single lost result silently disables
/// the background poll **and** every `Refresh`/`Retry` click for the rest of the process's life,
/// with the popover stuck on `checking…`: a failure mode with no error message and no way back.
///
/// Six [`crate::budget::fetch::REQUEST_TIMEOUT`]s (asserted against it at compile time, in that
/// module), so a request that is merely slow is never raced by a second one.
pub const IN_FLIGHT_STALE_AFTER: Duration = Duration::from_secs(60);

/// Whether this *build* compiles the provider poll in at all - `false` under `cfg(test)`.
///
/// **Off under `cfg(test)`, deliberately and non-negotiably.** The poll reads the *developer's
/// own* OAuth credential off disk and sends it to a real provider endpoint; a test suite that did
/// that would spend a real person's real rate-limit allowance (and hammer a limiter that is
/// already tight) every time anyone ran `cargo test`. Everything the loop does *around* the
/// network call - the single-flight guard, the manual-refresh floor, applying a result to state,
/// every derived readout - is tested directly against [`BudgetState::apply_read`], and the two
/// response parsers are tested against real payloads. The one thing not covered by a test is
/// "does the timer fire".
///
/// **This constant alone is not the guarantee** - see [`polling_enabled`], which is what every
/// caller actually asks. `cfg!(test)` is true only while the `app` crate is itself being compiled
/// *as* a test target; an integration test under `crates/app/tests/` links `app` as an ordinary
/// dependency, where this is `true` and the guard silently disarms.
pub const POLLING_ENABLED: bool = !cfg!(test);

/// Set this to anything (`JERRY_DISABLE_PROVIDER_POLL=1`) and no provider is ever read, in any
/// build.
///
/// The kill switch [`POLLING_ENABLED`] cannot be, because a compile-time `cfg` does not survive a
/// crate boundary. Any test harness that drives this app's UI - today's `#[gpui::test]`s already
/// get [`POLLING_ENABLED`], a future `crates/app/tests/` integration test would not - must set
/// this in its environment, and this repo's CI sets it for every job so a suite added there is
/// covered whether or not whoever adds it remembers. It is also the switch to reach for when
/// running the real app against a rate-limited account, or offline.
///
/// A `JERRY_`-prefixed environment variable rather than a Cargo feature for the same reason
/// `JERRY_REQUIRE_REAL_CLAUDE` (`crate::hooks::integration_tests`) is one: it works for a test
/// binary, a `cargo run`, a packaged build and a CI job identically, and needs no cooperation
/// from Cargo's feature resolution to reach a crate compiled as a plain dependency.
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
///
/// **Takes headroom, not usage**, even though usage is what every surface now prints - the
/// thresholds are a statement about how much budget is left, and passing a `% used` figure in
/// here would silently invert every colour on screen (a spent window would read green). The one
/// conversion lives in [`BudgetWindow::headroom_percent`], so no caller has to remember it.
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
    ///
    /// **Window label before the value**, like every other budget string in this app - §4c,
    /// verbatim: "`92% 5h` parsed as '92% for 5 hours'; `5h 92%` parses as '5-hour window, 92%'".
    /// That argument is about which half of the pair leads, and is untouched by the direction the
    /// number itself runs in.
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
    ///
    /// **Both floors live here, not in the caller.** The heartbeat used to hold its own cadence
    /// and ask this only about single-flight, which meant the rule that actually protects the
    /// provider's limiter was enforced by whichever code path happened to remember it. Every real
    /// read now passes one gate that knows both.
    ///
    /// The single-flight guard ages out after [`IN_FLIGHT_STALE_AFTER`] rather than being trusted
    /// forever - see that constant for the failure mode that buys.
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
///
/// One of these is the process's real budget ([`shared_budget`]); every window holds a *copy* of
/// it to render from. See that function for why the truth is process-global rather than per
/// window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetState {
    claude: ProviderBudget,
    codex: ProviderBudget,
}

impl BudgetState {
    /// Takes the right to start exactly one real read of `provider`, marking it in flight, or
    /// answers `false` because something else already holds that right (or because a click came
    /// inside [`MANUAL_REFRESH_FLOOR`]).
    ///
    /// Deliberately a claim rather than a question followed by a write: this runs against the
    /// process-global [`shared_budget`] with its lock held, so the check and the flag it sets are
    /// one indivisible step. Two windows waking their heartbeats at the same instant is the
    /// ordinary case, not a rare race.
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
    ///
    /// The three outcomes are kept genuinely distinct (rev 6 §7 rule 6):
    ///
    /// - **not connected** clears everything, including any numbers from before. A provider whose
    ///   credential has gone (a logout while Jerry was running) is not a provider with stale
    ///   numbers - the data is not ours to show any more.
    /// - **ok** replaces the numbers and clears the failure.
    /// - **failed** records the reason and *keeps* the previous numbers. They are still the last
    ///   true reading; §2's `last read <age>` takes over once they age past [`STALE_AFTER`], and
    ///   the failure itself surfaces as the `Retry` and as that row's tooltip.
    ///
    /// Every path clears [`ProviderBudget::in_flight`], including the failure one - that is the
    /// other half of [`claim_poll`], and the reason a panicked read is converted into a
    /// [`ProviderRead::Failed`] rather than allowed to swallow the result
    /// ([`crate::budget::fetch::read_provider_catching_panics`]).
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
///
/// # Why this is global rather than a field on each window
///
/// `File > New Window` (`crate::title_bar::menu`) builds a second, wholly independent `AdeApp`.
/// With the poll bookkeeping on that struct, every rate-limit rule this module has - the
/// [`POLL_INTERVAL`] cadence, the [`MANUAL_REFRESH_FLOOR`] on clicks, the single-flight guard -
/// would be per *window*, so N windows would read every provider N times as often. The endpoint
/// on the other end is the one that already answered a real `429` during issue #294's Phase 0
/// research (see [`POLL_INTERVAL`]'s own docs); multiplying its traffic by the number of windows
/// somebody happens to have open is exactly the "a budget readout that spent budget to fetch
/// itself" failure that constant exists to prevent.
///
/// The same shape `crate::sound::claim_app_start_sound` already uses for "once per process, not
/// once per window", for the same underlying reason: a process-wide fact cannot live in a
/// per-window struct.
///
/// Sharing the *results* too - rather than only claiming the right to poll and leaving other
/// windows blank - is what keeps every window's readout real: a second window renders the same
/// numbers the single shared poll fetched, refreshed onto its own copy by
/// `crate::budget::flow::AdeApp::sync_budget_from_shared`, instead of sitting on a permanent
/// `checking…` it is not allowed to resolve.
pub fn shared_budget() -> &'static Mutex<BudgetState> {
    static SHARED: OnceLock<Mutex<BudgetState>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(BudgetState::default()))
}

/// [`shared_budget`], locked, recovering rather than propagating if a previous holder panicked.
///
/// Poison is deliberately ignored: the data behind this lock is a readout, every field of it is
/// overwritten wholesale by the next poll, and there is no invariant a half-finished write could
/// break. Answering `unwrap()` here would turn one panic anywhere near the budget into a panic in
/// *every* window on every heartbeat afterwards - the same permanently-wedged failure mode
/// [`IN_FLIGHT_STALE_AFTER`] and the poll's own `catch_unwind` exist to rule out.
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
        let snapshot = snapshot(19.0, 60.0);
        assert_eq!(snapshot.windows[0].level(), BudgetLevel::Ok);
        assert_eq!(snapshot.windows[1].level(), BudgetLevel::Warn);
        assert_eq!(
            snapshot.tightest().map(|w| w.label.clone()),
            Some("7d".to_string()),
            "the week is the tight one here - and a single bar would have shown only that"
        );
    }

    /// §4c, verbatim: "`92% 5h` parsed as '92% for 5 hours'; `5h 92%` parses as '5-hour window,
    /// 92%'". The label leads - an argument about ordering that the direction of the number
    /// itself does not touch.
    #[test]
    fn every_window_string_puts_the_window_label_before_the_value() {
        let snapshot = snapshot(8.0, 35.0);
        assert_eq!(snapshot.summary_label(), "5h 8%  \u{b7}  7d 35%");
        assert!(
            snapshot.summary_label().starts_with("5h "),
            "the window label must lead - `8% 5h` reads as `8% for 5 hours`"
        );
    }

    /// **Every number on screen is percent used, and the bar agrees with it.** The design bundle
    /// specified the complement (`92% left`, on a meter where full is good) and this build shipped
    /// that first; the product owner overruled it after seeing it. Pinned here in one place so the
    /// flip cannot drift back a field at a time - and so the *hue*, which still keys off what is
    /// left, cannot be flipped along with the number by accident.
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

    /// The wedge this guards against was real: `in_flight` is set before a read starts and
    /// cleared when its result lands, so a result that never lands - a panicked parse, a dead
    /// executor thread - left it set forever, and [`ProviderBudget::may_poll_now`] then refused
    /// the background loop *and* every `Refresh`/`Retry` click for the rest of the session, with
    /// the popover stuck on `checking…` and no error anywhere.
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

    /// A landed result always clears the guard, on every one of the three outcomes - the ordinary
    /// half of the same contract.
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

    /// The whole of §4c's state machine, driven through the same entry point a real poll uses.
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

        // A logout mid-session: the credential is gone, and so are the numbers.
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

    /// §4c's rate-limit discipline is a rule about this *process*, not about one window: two
    /// windows sharing one [`BudgetState`] get one poll between them, not one each. Driven here
    /// against a local state standing in for [`shared_budget`], which is the same value the real
    /// windows claim against.
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

    /// The shared budget really is one value for the whole process - the property the paragraph
    /// above rests on, checked rather than assumed.
    #[test]
    fn the_shared_budget_is_a_single_process_wide_value() {
        assert!(
            std::ptr::eq(shared_budget(), shared_budget()),
            "every window must claim against the same state, or none of the rate-limit rules \
             mean anything across windows"
        );
    }

    /// The `cfg(test)` gate only covers this crate's *own* test targets. Anything else that drives
    /// this app - a future `crates/app/tests/` integration test, where `app` is compiled as a
    /// plain dependency with `cfg(test)` off - needs a switch that survives the crate boundary.
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
