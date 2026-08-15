//! Per-provider rate-limit budget: the agent pane's `claude 5h ▓▓▓▓▓▓░ 81% 7d ▓▓▓▓░░░ 40%`
//! readout and the popover behind it (GitHub issue #294,
//! `design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §2,
//! `STAGE-A-CHANGELOG.md` §4c/§4t/§4u′).
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free logic
//! apart from the `gpui::Div`-building code that draws it:
//!
//! - [`state`] - the provider set, the agent → provider mapping, the window/headroom model, the
//!   three-step hue thresholds and every string the two surfaces print. No `gpui::Window`, so
//!   the whole "which state is this provider really in" question is directly `#[test]`-able.
//! - [`fetch`] - real credential discovery on disk and the real HTTP read of each provider's own
//!   usage endpoint, plus the two response parsers. The parsers are pure functions over a
//!   `&str`, so they are tested against real captured/spec-derived payloads rather than against
//!   a live network.
//! - [`flow`] - the background poll loop, the manual `Refresh`/`Retry` path and the single-flight
//!   guards, as `impl AdeApp` methods.
//! - [`render`] - the pane strip's readout and the popover, as `impl AdeApp` methods.
//!
//! # Why this is per agent and not in the footer
//!
//! §4u′ is the last word in a four-pass argument: budget is a per-provider fact, a provider
//! belongs to an agent, and a fixed-width global bar cannot carry a readout whose count grows
//! with configuration. The pane is scoped to exactly one agent, which spends exactly one
//! provider - and it is also the right *moment*, because the pane is where you decide whether to
//! spend another turn. The footer's aggregate slot §4t kept was deleted outright by §4u′; the
//! accepted trade-off is that the popover is reachable only from an agent pane.
//!
//! # Two bars, not one - decided by what the payloads actually contain
//!
//! The design bundle disagreed with itself here (`REVISION-2026-08-14.md` §2 describes one meter
//! "filled to the tighter of the two windows"; `Jerry.dc.html` and §4u′ draw one bar per window),
//! and issue #294's Phase 0 spike resolved it against the real data rather than by picking a
//! document. **Both** supported providers expose two genuinely independent windows with their
//! own utilisation *and their own reset instant*: Claude's `five_hour`/`seven_day`, Codex's
//! `primary_window`/`secondary_window`. They do not move together - a session window refills in
//! hours, a week does not - so a single bar filled to the tighter one answers the reader's only
//! actionable question ("can I spend another turn right now?") with the wrong number whenever the
//! long window is the tight one. One bar per limit, each hued on its own value.
//!
//! # Nothing here is ever fabricated
//!
//! Every value on screen comes from a real HTTP read of a real provider endpoint, authorised with
//! the credential that provider's own CLI already stored on this machine. There is no synthetic
//! fallback: a provider with no credential reads `not connected`, one that has never been polled
//! says so, and a failed poll says `refresh failed` next to its `Retry` (rev 6 §7 rule 6 -
//! "converting a static span to a real input adds an empty state to everything derived from it").
//! A shell pane, which spends no provider at all, shows nothing.

use crate::root::menus;
use crate::root::*;
use crate::theme;
use gpui::{div, font, prelude::*, px, ClickEvent, Context, Window};

pub mod fetch;

pub mod state;

pub(crate) mod flow;

pub(crate) mod render;
