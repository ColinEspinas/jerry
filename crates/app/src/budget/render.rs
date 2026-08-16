//! The two surfaces: the agent pane strip's budget cluster, and the popover it opens.
//!
//! Both are `impl AdeApp` methods, like every other rendered surface in this crate, and both read
//! the same [`super::state::BudgetState`] - there is one derivation of "what does this provider
//! say right now" ([`super::state::ProviderBudget::readout`]) and two renderings of it, never two
//! derivations that could disagree.

use std::time::{Instant, SystemTime};

use super::state::{
    compact_age, BudgetLevel, BudgetWindow, Provider, ProviderReadout, ProviderSnapshot,
};
use super::*;
use crate::root::widgets::{menu_popover_chrome, text_tooltip};
use crate::status_bar::render::StatusTier;
use crate::status_bar::resources;
use crate::work_surface::agents::ProcessKind;

/// §4c: "Click opens a popover (292 wide, above the bar)".
const POPOVER_WIDTH: f32 = 292.0;

/// The pane strip's meter: `Jerry.dc.html`'s own `30x3` track, one per window.
const METER_WIDTH: f32 = 30.0;
const METER_HEIGHT: f32 = 3.0;

impl AdeApp {
    /// §4t's per-agent provider budget, right-aligned in the pane's readout strip:
    /// `claude 5h ▓░░░░░░ 19% 7d ▓▓▓▓░░░ 60%`, for **this** agent's provider - each meter filling
    /// with what has been *spent* (`super::state`'s "Used, not left").
    pub(crate) fn render_agent_budget_readout(
        &self,
        kind: ProcessKind,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let provider = super::state::provider_of(kind)?;
        let now = Instant::now();
        let budget = self.budget.get(provider);

        let mut cluster = div()
            .id("pty-footer-budget")
            .debug_selector(|| "pty-footer-budget".to_string())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .h(px(18.0))
            .px(px(4.0))
            .rounded(theme::radius::CHIP)
            .cursor_pointer()
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .when(self.budget_popover_open, |el| {
                el.bg(theme::surface::ROW_HOVER_ALT)
            })
            .tooltip(text_tooltip(match budget.readout(now) {
                ProviderReadout::Numbers(_) => format!(
                    "How much of {}'s rate limit is used - click for every window and provider",
                    provider.label()
                ),
                _ => format!("{}'s rate limit - click for details", provider.label()),
            }))
            // §4c's secondary tier is "provider names", verbatim - so the name is the one part of
            // this cluster that never carries a hue.
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .font_weight(StatusTier::Secondary.weight())
                    .text_size(self.ui_text_size(StatusTier::Secondary.text_size()))
                    .text_color(StatusTier::Secondary.color())
                    .child(provider.id()),
            );

        match budget.readout(now) {
            ProviderReadout::Numbers(snapshot) => {
                for (index, window) in snapshot.windows.iter().enumerate() {
                    cluster = cluster.child(self.render_budget_meter(index, window));
                }
            }
            ProviderReadout::LastRead(age) => {
                cluster = cluster.child(self.render_budget_state_text(
                    "pty-footer-budget-state",
                    format!("last read {} ago", compact_age(age)),
                    theme::budget::STALE,
                ));
            }
            ProviderReadout::RefreshFailed => {
                cluster = cluster.child(self.render_budget_state_text(
                    "pty-footer-budget-state",
                    "refresh failed".to_string(),
                    theme::budget::WARN,
                ));
            }
            ProviderReadout::NotConnected => {
                cluster = cluster.child(self.render_budget_state_text(
                    "pty-footer-budget-state",
                    "not connected".to_string(),
                    theme::budget::STALE,
                ));
            }
            ProviderReadout::Checking => {
                cluster = cluster.child(self.render_budget_state_text(
                    "pty-footer-budget-state",
                    "checking\u{2026}".to_string(),
                    theme::budget::STALE,
                ));
            }
        }

        Some(
            cluster
                .child({
                    let this = cx.entity();
                    gpui::canvas(
                        move |bounds, _window, cx| {
                            this.update(cx, |this, _cx| {
                                this.budget_readout_bounds = bounds;
                            });
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full()
                })
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    let opening = !this.budget_popover_open;
                    // GitHub issue #176's shared invariant: opening this popover closes whatever
                    // else was open. Read before the sweep and applied after it, because the sweep
                    // clears `budget_popover_open` itself.
                    let _ = this.close_menu_surfaces_except(Some(menus::MenuSurface::Budget));
                    this.budget_popover_open = opening;
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// One window's `5h ▓▓▓▓▓▓░ 81%` - label, then meter, then value, in that painted order.
    fn render_budget_meter(&self, index: usize, window: &BudgetWindow) -> impl IntoElement {
        let level = window.level();
        let label = window.label.clone();
        // The percentage only joins the attention family when it deserves to (§2) - a healthy
        // budget's number stays on the bar's own primary tier and spends no colour. Exactly the
        // rule `render_status_resources_readout` already applies to load.
        let value_color = if level == BudgetLevel::Ok {
            StatusTier::Primary.color()
        } else {
            level.color()
        };

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(5.0))
            .child(
                div()
                    .id(("budget-window-label", index))
                    .debug_selector(move || format!("budget-window-{index}-label"))
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::GHOST)
                    .child(label),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(METER_WIDTH))
                    .h(px(METER_HEIGHT))
                    .rounded(px(2.0))
                    .bg(theme::budget::TRACK)
                    .child(
                        div()
                            .h(px(METER_HEIGHT))
                            .w(gpui::relative(window.fill_fraction()))
                            .rounded(px(2.0))
                            .bg(level.color()),
                    ),
            )
            .child(
                div()
                    .id(("budget-window-value", index))
                    .debug_selector(move || format!("budget-window-{index}-value"))
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .text_color(value_color)
                    .child(window.value_label()),
            )
    }

    /// The strip's non-numeric states - `not connected`, `checking…`, `refresh failed`,
    /// `last read 3m ago`. One helper, so no state can quietly pick a different size or tier.
    fn render_budget_state_text(
        &self,
        selector: &'static str,
        text: String,
        color: theme::ColorToken,
    ) -> impl IntoElement {
        div()
            .id(selector)
            .debug_selector(move || selector.to_string())
            .flex_none()
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(10.0))
            .text_color(color)
            .child(text)
    }

    /// §4c/§4u′'s popover: the tightest provider on top with each window as a labelled meter and
    /// a reset countdown, the other providers with their own windows or their own failure state,
    /// and `Updated N ago · Refresh` at the foot.
    pub(crate) fn render_budget_popover(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = Instant::now();
        let anchor = self.budget_readout_bounds;
        let viewport = window.viewport_size();
        let width = px(POPOVER_WIDTH);

        // Opens upwards from the readout's own top edge. `right: -8px` in the mock is measured
        // from the readout, so the panel's right edge sits 8px past it; both edges are then
        // clamped into the window so a readout near either edge cannot push the panel off screen.
        let right_edge =
            (anchor.origin.x + anchor.size.width + px(8.0)).min(viewport.width - px(6.0));
        let left = (right_edge - width).max(px(6.0));
        let bottom = (viewport.height - anchor.origin.y + px(4.0)).max(px(6.0));

        let lead = self.budget.lead_provider(now);

        div()
            .id("budget-popover-scrim")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(crate::work_surface::state::TRANSPARENT)
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                // `stop_propagation` is what makes this a scrim rather than a transparent sheet -
                // see `render_resources_popover`'s own note: without it, clicking the readout to
                // dismiss would close here and immediately reopen there.
                cx.stop_propagation();
                this.budget_popover_open = false;
                cx.notify();
            }))
            .child(
                menu_popover_chrome(
                    div()
                        .id("budget-popover")
                        .debug_selector(|| "budget-popover".to_string())
                        .absolute()
                        .left(left)
                        .bottom(bottom)
                        .w(width)
                        .flex()
                        .flex_col(),
                    theme::shadow::MENU,
                )
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .child(self.render_budget_popover_header(cx))
                .children(lead.map(|provider| self.render_budget_lead_section(provider, now)))
                .child(self.render_budget_other_providers(lead, now, cx))
                .child(self.render_budget_popover_footer()),
            )
    }

    /// `RATE LIMITS` and the `Refresh` control.
    fn render_budget_popover_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(11.0))
            .pt(px(8.0))
            .pb(px(7.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_1()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::MUTED)
                    .child("RATE LIMITS"),
            )
            .child(
                div()
                    .id("budget-popover-refresh")
                    .debug_selector(|| "budget-popover-refresh".to_string())
                    .flex_none()
                    .cursor_pointer()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::button::BLUE_FG)
                    .hover(|el| el.text_color(theme::text::SELECTED))
                    .child("Refresh")
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        // A real read of every provider, not a label swap: §4c's `Refresh` "sets
                        // `Updated just now`", and here that line is derived from the instant a
                        // real result lands, so it can only say so once one has.
                        this.refresh_all_budgets(cx);
                    })),
            )
    }

    /// The lead provider's own block: its chip and name, then one labelled meter per window with
    /// its own reset countdown.
    fn render_budget_lead_section(&self, provider: Provider, now: Instant) -> gpui::AnyElement {
        let budget = self.budget.get(provider);
        let (chip_fg, chip_bg) = crate::work_surface::state::agent_tint(match provider {
            Provider::Claude => ProcessKind::claude(),
            Provider::Codex => ProcessKind::codex(),
        });

        let mut section = div()
            .px(px(11.0))
            .pt(px(9.0))
            .pb(px(10.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .pb(px(7.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(13.0))
                            .h(px(13.0))
                            .rounded(theme::radius::CHIP)
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(chip_bg)
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(8.0))
                            .text_color(chip_fg)
                            .child(provider.chip()),
                    )
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(11.5))
                            .text_color(theme::text::SELECTED)
                            .child(provider.label()),
                    ),
            );

        if let ProviderReadout::Numbers(snapshot) = budget.readout(now) {
            for window in &snapshot.windows {
                section = section.child(self.render_budget_lead_row(window));
            }
        }
        section.into_any_element()
    }

    /// One window inside the lead block: `5h · 8% used · resets in 4h 53m`, over its own meter.
    fn render_budget_lead_row(&self, window: &BudgetWindow) -> impl IntoElement {
        let level = window.level();
        let value_color = if level == BudgetLevel::Ok {
            theme::status_bar::PRIMARY
        } else {
            level.color()
        };

        div()
            .pt(px(5.0))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(30.0))
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(10.0))
                            .text_color(theme::text::DIM)
                            .child(window.label.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(10.5))
                            .text_color(value_color)
                            .child(window.popover_value_label()),
                    )
                    .children(window.reset_label(SystemTime::now()).map(|reset| {
                        div()
                            .flex_none()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(10.0))
                            .text_color(theme::text::FAINTER)
                            .child(reset)
                    })),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .h(px(4.0))
                    .w_full()
                    .rounded(px(2.0))
                    .bg(theme::budget::TRACK)
                    .child(
                        div()
                            .h(px(4.0))
                            .w(gpui::relative(window.fill_fraction()))
                            .rounded(px(2.0))
                            .bg(level.color()),
                    ),
            )
    }

    /// Every provider that is not the lead, each in whichever state it is really in - its own
    /// windows, `not connected`, `refresh failed` + `Retry`, or `last read <age>`.
    fn render_budget_other_providers(
        &self,
        lead: Option<Provider>,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let others: Vec<Provider> = Provider::ALL
            .into_iter()
            .filter(|provider| Some(*provider) != lead)
            .collect();

        div()
            .px(px(11.0))
            .pt(px(7.0))
            .pb(px(8.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div().pb(px(3.0)).child(
                    div()
                        .font(font(theme::font::SANS))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_size(self.ui_text_size(9.0))
                        .text_color(theme::status_bar::SECTION_LABEL)
                        // With no lead there is no "other" to be other *than* - the same rows
                        // under an honest label rather than one that presupposes a section that
                        // is not on screen.
                        .child(if lead.is_some() {
                            "OTHER PROVIDERS"
                        } else {
                            "PROVIDERS"
                        }),
                ),
            )
            .children(
                others
                    .into_iter()
                    .map(|provider| self.render_budget_other_row(provider, now, cx)),
            )
    }

    /// One provider's row. The `refresh failed` / `last read <age>` text carries the **real
    /// reason** the last attempt failed as its tooltip.
    fn render_budget_other_row(
        &self,
        provider: Provider,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let budget = self.budget.get(provider);
        let readout = budget.readout(now);
        // Only for a state the failure actually explains: a provider showing real, fresh numbers
        // may still hold a `last_error` from a newer attempt that failed, and hanging "the
        // provider answered 429" off numbers that are correct would read as though they were not.
        let failure_reason = match &readout {
            ProviderReadout::RefreshFailed | ProviderReadout::LastRead(_) => {
                budget.last_error.clone()
            }
            _ => None,
        };
        let (state_text, state_color) = match &readout {
            ProviderReadout::Numbers(snapshot) => (
                snapshot.summary_label(),
                Self::budget_summary_color(snapshot),
            ),
            ProviderReadout::LastRead(age) => (
                format!("last read {} ago", compact_age(*age)),
                theme::budget::STALE,
            ),
            ProviderReadout::RefreshFailed => ("refresh failed".to_string(), theme::budget::WARN),
            ProviderReadout::NotConnected => ("not connected".to_string(), theme::text::GHOST),
            ProviderReadout::Checking => ("checking\u{2026}".to_string(), theme::budget::STALE),
        };
        let name_color = match &readout {
            ProviderReadout::Numbers(_) => theme::text::STRONG,
            _ => theme::text::DIM,
        };
        let can_retry = budget.can_retry();

        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(20.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .text_color(name_color)
                    .child(provider.label()),
            )
            .child(
                div()
                    .id(match provider {
                        Provider::Claude => "budget-popover-state-claude",
                        Provider::Codex => "budget-popover-state-codex",
                    })
                    .debug_selector(move || format!("budget-popover-state-{}", provider.id()))
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(state_color)
                    .child(state_text)
                    .when_some(failure_reason, |el, reason| {
                        el.tooltip(text_tooltip(reason))
                    }),
            )
            .when(can_retry, |el| {
                el.child(
                    div()
                        .id(match provider {
                            Provider::Claude => "budget-popover-retry-claude",
                            Provider::Codex => "budget-popover-retry-codex",
                        })
                        .debug_selector(move || format!("budget-popover-retry-{}", provider.id()))
                        .flex_none()
                        .cursor_pointer()
                        .font(font(theme::font::SANS))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(self.ui_text_size(10.0))
                        .text_color(theme::button::BLUE_FG)
                        .hover(|el| el.text_color(theme::text::SELECTED))
                        .child("Retry")
                        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.refresh_provider_budget(provider, true, cx);
                        })),
                )
            })
    }

    /// A summary row's hue: its own tightest window's, so a provider one divider away from the
    /// lead still reports honestly - and stays neutral while it is healthy.
    fn budget_summary_color(snapshot: &ProviderSnapshot) -> theme::ColorToken {
        match snapshot.tightest().map(|window| window.level()) {
            Some(BudgetLevel::Ok) | None => theme::text::DIM,
            Some(level) => level.color(),
        }
    }

    /// §4c's foot: `Updated 3m ago` and the footnote `counts spend, not headroom` - the design
    /// bundle's own `counts headroom, not spend`, inverted along with the numbers it describes
    /// (`super::state`'s "Used, not left").
    fn render_budget_popover_footer(&self) -> impl IntoElement {
        let since = self
            .budget
            .last_read_at()
            .map(|at| Instant::now().saturating_duration_since(at));
        div()
            .flex()
            .items_center()
            .px(px(11.0))
            .pt(px(7.0))
            .pb(px(8.0))
            .child(
                div()
                    .flex_1()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::status_bar::RECESSIVE)
                    .child(resources::updated_ago_label(since)),
            )
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::HINT)
                    .child("counts spend, not headroom"),
            )
    }
}
