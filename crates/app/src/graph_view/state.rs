//! Pure state and formatting for the git graph tab - see `super`'s module docs.

use crate::root::AdeApp;
use crate::text_history::TextField;
use crate::theme;
use gpui::{Bounds, Context, FocusHandle, Pixels};
use std::collections::HashMap;
use wt_core::graph::{Graph, GraphScope};

/// Which side panel the graph tab's right sidebar shows while it's focused, replacing
/// Files/Changes (design spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum GraphRightPanel {
    #[default]
    Commit,
    Branches,
}

/// The graph tab's real background load, mirroring `crate::code_surface::state::DiffLoadState`'s
/// shape.
#[derive(Debug, Clone, Default)]
pub(crate) enum GraphLoadState {
    /// The tab has never been opened yet, so nothing has been loaded (deliberately not eager at
    /// app startup - a `gix` commit walk is real blocking I/O, not worth paying for a tab that
    /// may never open).
    #[default]
    NotLoaded,
    Loading,
    Loaded(Graph),
    Error(String),
}

/// The graph tab's own UI state: what's loaded, which scope/panel is selected, and the row `⋯`/
/// Push `▾` menu popovers' anchors. One instance, owned by [`AdeApp::graph_state`].
pub(crate) struct GraphTabState {
    pub load: GraphLoadState,
    pub scope: GraphScope,
    pub right_panel: GraphRightPanel,
    /// Index into the loaded [`Graph`]'s `rows` - the selected commit row (Commit panel target).
    pub selected_row: Option<usize>,
    /// The Commit panel's real "Files changed" list for whichever commit sha this names - loaded
    /// on a background thread (`crate::graph_view::render::AdeApp::load_commit_files`) since
    /// `wt_core::graph::commit_changed_files` performs real blocking I/O (spawns `git show`) and
    /// must never run inline in a render method. Keyed by sha rather than row index so a stale
    /// entry is detectable even across a reload that renumbers rows. `Err` is a real error
    /// message, not silently swallowed into an empty list.
    pub commit_files_cache: Option<(
        String,
        Result<Vec<wt_core::graph::CommitFileChange>, String>,
    )>,
    /// Which row's `⋯` context menu is open, if any (an index into `rows`, not a stateful id -
    /// see `super::render`'s docs on why this is safe: the menu closes on any reload).
    pub row_menu_open: Option<usize>,
    /// Every currently-rendered row's own `⋯` trigger bounds, captured by a `gpui::canvas` child
    /// each render - keyed by row index, unlike `AdeApp::plus_button_bounds`'s single field,
    /// because (unlike the tab strip's one `+` button) every row's trigger paints every frame
    /// simultaneously; a single shared field would be overwritten by whichever row happened to
    /// paint last, not necessarily the row whose menu is actually open.
    pub row_menu_bounds: HashMap<usize, Bounds<Pixels>>,
    pub push_menu_open: bool,
    pub push_button_bounds: Bounds<Pixels>,
    /// The Branches panel's real filter box - a genuine text-input surface, so it carries the
    /// project's `"text-input"` keybinding-context tag (see `super::render`) exactly like
    /// `crate::rail::render`'s agent filter and `crate::settings::render`'s keymap filter.
    pub branches_filter: TextField,
    pub branches_filter_focus_handle: FocusHandle,
    /// Real ahead/behind counts against `HEAD`'s configured upstream
    /// (`wt_core::graph::ahead_behind_against_upstream`), refreshed alongside every
    /// `Self::load` reload - the toolbar's `Pull ↓N` / `Push ↑N` counts. `None` while loading or
    /// when there's genuinely no upstream to compare against, never a fabricated `{0, 0}`.
    pub upstream_counts: Option<wt_core::diff::AheadBehind>,
    /// An honest, real status line for a not-yet-wired toolbar action (Fetch/Pull/Push) - "not
    /// implemented yet", never a fake success. `None` when nothing has been clicked since the
    /// tab last opened.
    pub status_message: Option<String>,
}

impl GraphTabState {
    pub(crate) fn new(cx: &mut Context<AdeApp>) -> Self {
        Self {
            load: GraphLoadState::default(),
            scope: GraphScope::default(),
            right_panel: GraphRightPanel::default(),
            selected_row: None,
            commit_files_cache: None,
            row_menu_open: None,
            row_menu_bounds: HashMap::new(),
            push_menu_open: false,
            push_button_bounds: Bounds::default(),
            branches_filter: TextField::new(),
            branches_filter_focus_handle: cx.focus_handle(),
            upstream_counts: None,
            status_message: None,
        }
    }
}

/// The lane canvas's `x` position for `lane`'s vertical (design spec §2: `x = 9 + lane * 14`).
pub(crate) fn lane_x(lane: usize) -> Pixels {
    theme::graph::LANE_X_BASE + theme::graph::LANE_STEP * (lane as f32)
}

/// One of the six lane colours, cycled by `lane % 6` (design spec §2).
pub(crate) fn lane_color(lane: usize) -> gpui::Rgba {
    theme::graph::LANES[lane % theme::graph::LANES.len()].into()
}

/// A local branch ref chip's dim background, cycled the same way as [`lane_color`].
pub(crate) fn local_branch_dim_bg(lane: usize) -> gpui::Rgba {
    theme::graph::LOCAL_BRANCH_DIM_BG[lane % theme::graph::LOCAL_BRANCH_DIM_BG.len()].into()
}

/// Formats a real Unix timestamp (a commit's committer time) as the graph row's relative-time
/// column (design spec §2: "now · 2m · 4m · 6m · 8m · 18m · 2h · ... · 4d"). `now` is passed in
/// (rather than read from `std::time::SystemTime::now()` internally) so this stays a pure,
/// deterministically testable function.
pub(crate) fn relative_time(commit_unix_seconds: i64, now_unix_seconds: i64) -> String {
    let delta = (now_unix_seconds - commit_unix_seconds).max(0);
    if delta < 60 {
        return "now".to_string();
    }
    if delta < 3600 {
        return format!("{}m", delta / 60);
    }
    if delta < 86400 {
        return format!("{}h", delta / 3600);
    }
    format!("{}d", delta / 86400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_formats_each_bucket() {
        let now = 1_700_100_000;
        assert_eq!(relative_time(now, now), "now");
        assert_eq!(relative_time(now - 59, now), "now");
        assert_eq!(relative_time(now - 60, now), "1m");
        assert_eq!(relative_time(now - 2 * 60, now), "2m");
        assert_eq!(relative_time(now - 3600, now), "1h");
        assert_eq!(relative_time(now - 2 * 3600, now), "2h");
        assert_eq!(relative_time(now - 86400, now), "1d");
        assert_eq!(relative_time(now - 4 * 86400, now), "4d");
    }

    #[test]
    fn relative_time_never_goes_negative_for_a_future_timestamp() {
        // A commit's clock could theoretically be slightly ahead of ours; must not panic or
        // print a negative duration.
        assert_eq!(relative_time(1_700_100_100, 1_700_100_000), "now");
    }

    #[test]
    fn lane_x_matches_the_spec_formula() {
        assert_eq!(lane_x(0), theme::graph::LANE_X_BASE);
        assert_eq!(
            lane_x(1),
            theme::graph::LANE_X_BASE + theme::graph::LANE_STEP
        );
    }

    #[test]
    fn lane_color_cycles_through_all_six() {
        assert_eq!(
            lane_color(0),
            lane_color(6),
            "lane colours must cycle every 6 lanes"
        );
        assert_eq!(lane_color(1), lane_color(7));
        assert_ne!(lane_color(0), lane_color(1), "adjacent lanes must differ");
    }
}
