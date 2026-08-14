//! Pure state and formatting for the git graph tab - see `super`'s module docs.

use super::rebase::RebaseModeState;
use crate::root::plural;
use crate::root::AdeApp;
use crate::text_history::TextField;
use crate::theme;
use gpui::{Bounds, Context, FocusHandle, Pixels, ScrollHandle, Task, UniformListScrollHandle};
use std::collections::HashMap;
use wt_core::graph::{Graph, GraphScope};
use wt_core::remote::PushForce;

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

/// An open row `⋯`/right-click context menu: which row it targets, and the already-resolved
/// window-space origin its popover paints at - captured once at open time from the real click (a
/// right-click anywhere on the row) or the `⋯` trigger button's own captured bounds, never
/// recomputed from the row's index or a per-row-index formula. Mirrors
/// `crate::sidebar::tree_ops::TreeContextMenu`'s identical "resolve once, at open time" shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GraphRowMenu {
    pub row_index: usize,
    pub origin_x: Pixels,
    pub origin_y: Pixels,
}

/// Which of the two row-menu actions a [`GraphCreateBranchPrompt`] is collecting a name for
/// (GitHub issue #241). Both ask the same question ("what should this branch be called?") of the
/// same commit, through the same hand-rolled input, and differ only in what happens on Enter -
/// so they share one prompt rather than growing a second, near-identical overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphBranchPromptKind {
    /// "Create branch here" - `wt_core::checkout::create_branch_at` in the focused worktree.
    CreateBranch,
    /// "Start agent from this commit" - a real new `git worktree add -b <name> -- <path> <sha>`
    /// followed by spawning an agent in it. See
    /// `crate::graph_view::render::AdeApp::request_graph_start_agent_from_commit`.
    StartAgent,
}

/// GitHub issue #241: the row menu's "Create branch here" prompt - `Some` only while the small,
/// hand-rolled branch-name modal (`crate::graph_view::render::AdeApp::
/// render_graph_create_branch_prompt`) is open. Mirrors `crate::root::new_file::
/// NewFileInputState`'s own shape - this app's one established "prompt for a name" idiom
/// (append/backspace-only [`TextField`], Enter to confirm, Escape to cancel) - rather than a
/// second, competing one.
///
/// `sha`/`short_sha`/`subject` are captured once, at open time, from the row that was
/// right-clicked/`⋯`'d - not re-looked-up from the graph on every render, which a background
/// reload racing with the still-open prompt could otherwise change out from under it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GraphCreateBranchPrompt {
    /// Which action opened this prompt - see [`GraphBranchPromptKind`].
    pub kind: GraphBranchPromptKind,
    pub sha: String,
    pub short_sha: String,
    pub subject: String,
    /// For [`GraphBranchPromptKind::StartAgent`] only: the real parent directory the new
    /// worktree will be created under, resolved once at open time (see
    /// `crate::graph_view::render::graph_new_worktree_parent`). Rendered in the prompt so the
    /// destination is never a silent guess the user only discovers afterwards - the app has no
    /// persisted "worktree root" setting to consult (see `crate::settings::state`'s own module
    /// docs on why), so the honest alternative to showing it is not showing the action at all.
    pub worktree_parent: Option<std::path::PathBuf>,
    /// A real rejection message from the last attempt - only this prompt's own "branch name
    /// can't be empty" guard (a real collision with an existing branch surfaces through
    /// [`GraphTabState::status_message`] instead, git's own real error text, exactly like every
    /// other row-menu mutation) - cleared on the very next keystroke.
    pub error: Option<String>,
}

/// Everything [`graph_merge_gate`] is allowed to look at, already resolved into plain values by
/// its one caller (`crate::graph_view::render::AdeApp::graph_merge_facts`). Keeping the decision
/// a pure function of these - rather than reaching into `AdeApp` from inside it - is what makes
/// every precondition below unit-testable without a GPUI window, the same split
/// `crate::rail::state::prunable_worktree_paths` already uses for the rail's own destructive
/// action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphMergeFacts {
    /// The focused worktree's own checked-out branch - `None` on a detached `HEAD`, which has no
    /// branch for `git merge` to name as the source.
    pub head_branch: Option<String>,
    /// The repository's detected default/base branch (`wt_core::diff::WorktreeDiff::base_branch`),
    /// as already loaded for the focused worktree - `None` when none could be detected.
    pub base_branch: Option<String>,
    /// How many real uncommitted paths the focused worktree has
    /// (`wt_core::stage::dirty_paths`, via [`AdeApp::dirty_files`]).
    pub uncommitted_files: usize,
    /// How many real agent sessions in the focused worktree are still `Run`ning or `Ask`ing.
    pub live_agents: usize,
    /// Whether the focused worktree has any agent tab at all for the existing conflict resolver
    /// to render in - see [`GraphMergeGate::Blocked`]'s own docs and
    /// `crate::merge::flow::AdeApp::start_merge`.
    pub has_agent_tab: bool,
    /// `AdeApp::merge_flow.is_some()` - only one merge runs at a time app-wide.
    pub merge_already_running: bool,
}

/// Whether the graph's "Merge into `<base>`" control is live, and if not, the real reason - shown
/// on the control itself, never as a silent no-op (GitHub issue #241;
/// `design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §1 rule 3: "disabled while
/// anything is uncommitted, with the reason on the button itself").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphMergeGate {
    /// Every precondition holds.
    Ready {
        head_branch: String,
        base_branch: String,
    },
    /// A real, user-facing reason this merge cannot run right now.
    Blocked(String),
}

impl GraphMergeGate {
    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, GraphMergeGate::Ready { .. })
    }

    /// The reason string for a blocked gate - `None` when ready, so a caller can never render a
    /// reason next to a live control.
    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            GraphMergeGate::Ready { .. } => None,
            GraphMergeGate::Blocked(reason) => Some(reason.as_str()),
        }
    }
}

/// The one place the graph decides whether "Merge into `<base>`" may run (GitHub issue #241).
///
/// The two *content* preconditions and their exact wording come straight from the design's own
/// `mergeReady`/`mergeWhy` (`design_handoff_jerry_ade/revision 5/Jerry.dc.html`):
/// `!uncommitted.length && !liveAgents.length`, reported as `N files still uncommitted` /
/// `N agents still working`. The structural ones ahead of them are this app's own: they describe
/// states in which `wt_core::merge::attempt_merge` could not do anything meaningful at all, and
/// each mirrors a real `wt_core::Error` variant that function would otherwise return only *after*
/// the user clicked (`MergeSourceDetached`, `MergeNoBaseBranch`, `MergeSourceIsBaseBranch`) -
/// checked here so the reason is visible on the control instead of arriving as an error.
///
/// Order is deliberate: a structurally impossible merge outranks a merely blocked one, because
/// "3 files still uncommitted" on a detached `HEAD` would imply that committing them is enough to
/// unblock it, which is false.
pub(crate) fn graph_merge_gate(facts: &GraphMergeFacts) -> GraphMergeGate {
    let Some(head_branch) = facts.head_branch.clone() else {
        return GraphMergeGate::Blocked("detached HEAD \u{2013} no branch to merge".to_string());
    };
    let Some(base_branch) = facts.base_branch.clone() else {
        return GraphMergeGate::Blocked("no base branch detected".to_string());
    };
    if head_branch == base_branch {
        return GraphMergeGate::Blocked(format!("already on {base_branch}"));
    }
    if facts.merge_already_running {
        return GraphMergeGate::Blocked("a merge is already running".to_string());
    }
    if facts.uncommitted_files > 0 {
        return GraphMergeGate::Blocked(format!(
            "{} still uncommitted",
            plural::count(facts.uncommitted_files, "file", None)
        ));
    }
    if facts.live_agents > 0 {
        return GraphMergeGate::Blocked(format!(
            "{} still working",
            plural::count(facts.live_agents, "agent", None)
        ));
    }
    if !facts.has_agent_tab {
        // The conflict resolver this action hands off to renders inside an agent's own work
        // surface (`crate::merge::render::AdeApp::render_merge_flow_surface` takes an `&Agent`,
        // reached only from `crate::work_surface::render`'s active-agent branch), so a worktree
        // with no agent tab at all has nowhere to show a conflicted merge. Refusing up front is
        // the honest reading of that: the alternative is starting a merge whose conflicts would
        // be invisible.
        return GraphMergeGate::Blocked("no agent in this worktree to merge in".to_string());
    }
    GraphMergeGate::Ready {
        head_branch,
        base_branch,
    }
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
    /// Which row's `⋯`/right-click context menu is open, if any, and the real position it was
    /// opened at (see [`GraphRowMenu`]) - not a stateful id (see `super::render`'s docs on why an
    /// index is safe: the menu closes on any reload).
    pub row_menu_open: Option<GraphRowMenu>,
    /// Every currently-rendered row's own `⋯` trigger bounds, captured by a `gpui::canvas` child
    /// each render - keyed by row index, unlike `AdeApp::plus_button_bounds`'s single field,
    /// because (unlike the tab strip's one `+` button) every row's trigger paints every frame
    /// simultaneously; a single shared field would be overwritten by whichever row happened to
    /// paint last, not necessarily the row whose menu is actually open. Used to anchor the popover
    /// when the menu is opened from the `⋯` button itself; a right-click instead anchors off the
    /// real click position (`GraphRowMenu::origin_x`/`origin_y`), which needs no bounds lookup.
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
    /// A real, live status line for the toolbar's Fetch/Pull/Push actions - success or a real
    /// git error message, never a fake success. `None` when nothing has been clicked since the
    /// tab last opened.
    pub status_message: Option<String>,
    /// `true` while a real `wt_core::remote::{fetch,pull,push}` call is running on the
    /// background executor - guards Fetch/Pull/Push against a double-click starting a second,
    /// overlapping git subprocess against the same worktree, mirroring
    /// `AdeApp::worktree_history_op_in_flight`'s identical single-flight discipline for
    /// Keep/Discard/Commit (a separate flag, not the same field: a graph-tab remote operation
    /// and a Changes-panel worktree-history operation are never mutually exclusive with each
    /// other, only with themselves).
    pub remote_op_in_flight: bool,
    /// `Some(force)` for exactly one real click past the two-click confirmation on the Push
    /// menu's "Force with lease"/"Force" rows (both real, remote-history-losing operations -
    /// see `wt_core::remote::PushForce::Force`'s own docs) - `None` for the ordinary, single-
    /// click "Push" row, which is never destructive to already-pushed history. Mirrors
    /// `crate::rail::render::AdeApp::request_prune`'s own two-click discipline: the *first*
    /// click on a force row only arms this field and re-labels the row, without pushing
    /// anything; only a second click on the *same* `force` value actually runs
    /// [`wt_core::remote::push`]. Clicking a different row (including the plain "Push" row)
    /// disarms this rather than carrying the arm over onto an operation the user never
    /// confirmed.
    pub push_force_confirm_armed: Option<PushForce>,
    /// GitHub issue #241: `Some(sha)` for exactly one real click past the two-click confirmation
    /// on the row menu's "Hard" reset row, naming the commit that click targeted - `None` for
    /// "Soft"/"Mixed", which never discard uncommitted work and so need no confirmation at all.
    /// Mirrors [`Self::push_force_confirm_armed`]'s own discipline (see that field's docs): the
    /// *first* click on a given commit's "Hard" row only arms this field and re-labels the row,
    /// without resetting anything; only a second click on that *same* commit's "Hard" row - with
    /// nothing else clicked in between - actually runs [`wt_core::checkout::reset`]. Keyed by
    /// sha rather than a bare `bool` for the same reason `push_force_confirm_armed` is keyed by
    /// variant: a "Hard" click on a *different* commit must arm its own confirmation, not
    /// silently ride on a stale arm left over from a different row. Every other row-menu action
    /// (Check out, Create branch here, Cherry-pick, Revert, Rebase onto this commit, Soft/Mixed
    /// reset, Copy) disarms this rather than let it carry over onto an operation the user never
    /// confirmed - see `crate::graph_view::render::AdeApp::request_graph_reset`'s own docs.
    pub hard_reset_confirm_armed: Option<String>,
    /// GitHub issue #241: the row menu's "Create branch here" prompt - see
    /// [`GraphCreateBranchPrompt`]'s own docs.
    pub create_branch_prompt: Option<GraphCreateBranchPrompt>,
    /// The prompt's own real text input - a real undo history (GitHub issue #17), the same shape
    /// as [`Self::branches_filter`]. Lives independently of [`Self::create_branch_prompt`]
    /// (rather than nested inside it) only so it can be reset to empty with `TextField::new()`
    /// each time the prompt opens without reconstructing the whole prompt struct around it.
    pub create_branch_name: TextField,
    pub create_branch_focus_handle: FocusHandle,
    /// The currently in-flight remote operation's own real task - held so it isn't dropped (and
    /// therefore cancelled) the instant this function returns, matching
    /// `AdeApp::_worktree_history_task`'s identical one-slot-per-feature pattern. `None` when
    /// [`Self::remote_op_in_flight`] is `false`.
    pub _remote_op_task: Option<Task<()>>,
    /// GitHub issue #221 ("Git graph only displays 500 commits"). The `max_commits` cap the
    /// currently loaded [`Graph`] was really walked with - `wt_core::graph::DEFAULT_MAX_COMMITS`
    /// after a fresh [`AdeApp::load_graph`], then one
    /// `crate::graph_view::render::LOAD_MORE_BATCH` higher per completed "load more".
    ///
    /// The next cap is derived from *this*, not from `graph.rows.len()`, for two real reasons:
    /// `rows` can carry a synthetic "Uncommitted changes" row that was never subject to the cap
    /// at all (`wt_core::graph::build_graph`), and deriving it from the last *requested* cap makes
    /// the sequence strictly monotonic, so even a history that shrinks under us (an amend, a
    /// rebase, a `gc` between two walks) still terminates instead of ping-ponging between two
    /// caps forever.
    pub loaded_cap: usize,
    /// `true` while a real "load more" walk is running on the background executor. Guards the row
    /// builder - which runs several times per frame, on every frame the user is scrolled near the
    /// bottom - against spawning a second, overlapping `build_graph` over the same history, the
    /// same single-flight discipline [`Self::remote_op_in_flight`] applies to Fetch/Pull/Push.
    pub load_more_in_flight: bool,
    /// `true` once a "load more" walk has genuinely failed. Without it the trigger would re-fire
    /// on the very next frame and spin a failing `gix` walk forever; the real error is reported
    /// once through [`Self::status_message`] instead. Cleared by any fresh
    /// [`AdeApp::load_graph`], which is also the only way to retry.
    pub load_more_failed: bool,
    /// The in-flight "load more" task - held so dropping it (and therefore cancelling the walk)
    /// doesn't happen the instant the spawning function returns, exactly like
    /// [`Self::_remote_op_task`]. `None` when [`Self::load_more_in_flight`] is `false`.
    pub _load_more_task: Option<Task<()>>,
    /// GitHub issue #142: the commit row list's own scroll position, tracked so
    /// `crate::root::scrollbar::AdeApp::render_vertical_scrollbar` has a real handle to draw
    /// against - every other scrollable region in the app has had one since GitHub issue #30;
    /// the graph tab shipped after that audit and was never retrofitted.
    ///
    /// A `UniformListScrollHandle` rather than a plain `ScrollHandle` since GitHub issue #218:
    /// the row list is a real `gpui::uniform_list` now, which owns its own scroll offset and
    /// tracks it through this type (`vendor/zed/crates/gpui/src/elements/uniform_list.rs`).
    /// `crate::root::scrollbar::ScrollableHandle` is implemented for both kinds, so the
    /// scrollbar call site is unchanged - see that trait's own docs.
    pub rows_scroll_handle: UniformListScrollHandle,
    /// The Commit panel's own scroll position - see [`Self::rows_scroll_handle`]'s docs.
    pub commit_panel_scroll_handle: ScrollHandle,
    /// The Branches panel's own scroll position - see [`Self::rows_scroll_handle`]'s docs.
    pub branches_scroll_handle: ScrollHandle,
    /// GitHub issue #242 phase B: `Some` only while the graph pane is showing its interactive-
    /// rebase mode (design spec §1) instead of its ordinary commit list - see
    /// `crate::graph_view::rebase`'s own module docs.
    pub rebase: Option<RebaseModeState>,
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
            remote_op_in_flight: false,
            push_force_confirm_armed: None,
            hard_reset_confirm_armed: None,
            create_branch_prompt: None,
            create_branch_name: TextField::new(),
            create_branch_focus_handle: cx.focus_handle(),
            _remote_op_task: None,
            loaded_cap: wt_core::graph::DEFAULT_MAX_COMMITS,
            load_more_in_flight: false,
            load_more_failed: false,
            _load_more_task: None,
            rows_scroll_handle: UniformListScrollHandle::new(),
            commit_panel_scroll_handle: ScrollHandle::new(),
            branches_scroll_handle: ScrollHandle::new(),
            rebase: None,
        }
    }
}

/// The lane canvas's `x` position for `lane`'s vertical (design spec §2: `x = 9 + lane * 14`).
pub(crate) fn lane_x(lane: usize) -> Pixels {
    theme::graph::LANE_X_BASE + theme::graph::LANE_STEP * (lane as f32)
}

/// The real, per-graph lane canvas width - `theme::graph::LANE_CANVAS`'s fixed 100px only fits
/// up to `lane_count == 7` (`lane_x(6) == 93`, plus a little breathing room past the last lane's
/// own dot); a repository with more concurrent branches than that previously had its rightmost
/// lanes' dots and elbows painted past the canvas's own right edge, directly overlapping the ref
/// chips and subject text columns next to it (a real user report). Grows past the fixed default
/// exactly as far as `lane_count` actually needs, and never shrinks below it - a graph with few
/// lanes keeps the same familiar width it always had.
pub(crate) fn graph_lane_canvas_width(lane_count: usize) -> Pixels {
    let last_lane = lane_count.saturating_sub(1);
    let needed = lane_x(last_lane) + theme::graph::LANE_X_BASE;
    needed.max(theme::graph::LANE_CANVAS)
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
    fn graph_lane_canvas_width_never_shrinks_below_the_fixed_default() {
        assert_eq!(graph_lane_canvas_width(0), theme::graph::LANE_CANVAS);
        assert_eq!(graph_lane_canvas_width(1), theme::graph::LANE_CANVAS);
        // Real, currently-fitting lane counts must not grow the canvas past its familiar
        // default - only a repository with genuinely more concurrent lanes than that should.
        for lane_count in 2..=6 {
            assert_eq!(
                graph_lane_canvas_width(lane_count),
                theme::graph::LANE_CANVAS,
                "lane_count {lane_count} should still fit inside the existing default width"
            );
        }
    }

    #[test]
    fn graph_lane_canvas_width_grows_to_fit_more_lanes_than_the_default_holds() {
        // A real user report: with enough concurrent branches, the fixed 100px canvas let the
        // rightmost lanes' dots/elbows paint past its own right edge, directly overlapping the
        // ref chips and subject text columns next to it.
        let width = graph_lane_canvas_width(12);
        assert!(
            width > theme::graph::LANE_CANVAS,
            "12 real concurrent lanes must grow the canvas past its fixed default: got {width:?}"
        );
        // The widened canvas must actually fit the rightmost lane's own dot, not just be "wider
        // than default" by an arbitrary amount - real coverage past `lane_x(11)`.
        assert!(
            width > lane_x(11),
            "canvas width {width:?} must extend past the rightmost lane's own x position {:?}",
            lane_x(11)
        );
    }

    /// Every precondition holding - the one shape [`graph_merge_gate`] is allowed to return
    /// `Ready` for. Each test below starts from this and breaks exactly one thing.
    fn ready_facts() -> GraphMergeFacts {
        GraphMergeFacts {
            head_branch: Some("feature".to_string()),
            base_branch: Some("main".to_string()),
            uncommitted_files: 0,
            live_agents: 0,
            has_agent_tab: true,
            merge_already_running: false,
        }
    }

    #[test]
    fn merge_gate_is_ready_only_when_every_precondition_holds() {
        assert_eq!(
            graph_merge_gate(&ready_facts()),
            GraphMergeGate::Ready {
                head_branch: "feature".to_string(),
                base_branch: "main".to_string(),
            }
        );
        assert!(graph_merge_gate(&ready_facts()).is_ready());
        assert_eq!(graph_merge_gate(&ready_facts()).reason(), None);
    }

    /// The design's own `mergeWhy` wording, through the shared pluralisation helper - `1 file`,
    /// never `1 files` (`REVISION-2026-08-14.md` §7 rule 9).
    #[test]
    fn merge_gate_reports_uncommitted_files_with_real_pluralisation() {
        let one = GraphMergeFacts {
            uncommitted_files: 1,
            ..ready_facts()
        };
        assert_eq!(
            graph_merge_gate(&one).reason(),
            Some("1 file still uncommitted")
        );
        let many = GraphMergeFacts {
            uncommitted_files: 13,
            ..ready_facts()
        };
        assert_eq!(
            graph_merge_gate(&many).reason(),
            Some("13 files still uncommitted")
        );
        assert!(!graph_merge_gate(&many).is_ready());
    }

    #[test]
    fn merge_gate_reports_live_agents_with_real_pluralisation() {
        let one = GraphMergeFacts {
            live_agents: 1,
            ..ready_facts()
        };
        assert_eq!(
            graph_merge_gate(&one).reason(),
            Some("1 agent still working")
        );
        let two = GraphMergeFacts {
            live_agents: 2,
            ..ready_facts()
        };
        assert_eq!(
            graph_merge_gate(&two).reason(),
            Some("2 agents still working")
        );
    }

    /// A structurally impossible merge must never be reported as a merely-blocked one: saying
    /// "3 files still uncommitted" on a detached `HEAD` would imply committing them unblocks it,
    /// which is false - there is no branch for `git merge` to name as the source either way.
    #[test]
    fn merge_gate_puts_structural_refusals_ahead_of_content_ones() {
        let detached_and_dirty = GraphMergeFacts {
            head_branch: None,
            uncommitted_files: 3,
            live_agents: 2,
            ..ready_facts()
        };
        assert_eq!(
            graph_merge_gate(&detached_and_dirty).reason(),
            Some("detached HEAD \u{2013} no branch to merge")
        );

        let no_base_and_dirty = GraphMergeFacts {
            base_branch: None,
            uncommitted_files: 3,
            ..ready_facts()
        };
        assert_eq!(
            graph_merge_gate(&no_base_and_dirty).reason(),
            Some("no base branch detected")
        );
    }

    /// `wt_core::merge::attempt_merge` returns `Error::MergeSourceIsBaseBranch` for this, i.e.
    /// the click could only ever fail - the reason belongs on the control instead.
    #[test]
    fn merge_gate_refuses_merging_the_base_branch_into_itself() {
        let on_base = GraphMergeFacts {
            head_branch: Some("main".to_string()),
            base_branch: Some("main".to_string()),
            ..ready_facts()
        };
        assert_eq!(graph_merge_gate(&on_base).reason(), Some("already on main"));
    }

    #[test]
    fn merge_gate_refuses_a_second_concurrent_merge() {
        let running = GraphMergeFacts {
            merge_already_running: true,
            ..ready_facts()
        };
        assert_eq!(
            graph_merge_gate(&running).reason(),
            Some("a merge is already running")
        );
    }

    /// The conflict resolver renders inside an agent's own work surface, so a worktree with no
    /// agent tab has nowhere to show a conflicted merge - see [`graph_merge_gate`]'s own docs.
    #[test]
    fn merge_gate_refuses_when_there_is_no_agent_tab_to_show_a_conflict_in() {
        let no_agent = GraphMergeFacts {
            has_agent_tab: false,
            ..ready_facts()
        };
        assert_eq!(
            graph_merge_gate(&no_agent).reason(),
            Some("no agent in this worktree to merge in")
        );
    }

    /// A blocked gate must never also look ready, and a ready one must never carry a reason -
    /// the two accessors are what the render path branches on, so they cannot disagree.
    #[test]
    fn merge_gate_ready_and_reason_never_disagree() {
        for facts in [
            ready_facts(),
            GraphMergeFacts {
                uncommitted_files: 1,
                ..ready_facts()
            },
            GraphMergeFacts {
                head_branch: None,
                ..ready_facts()
            },
        ] {
            let gate = graph_merge_gate(&facts);
            assert_eq!(
                gate.is_ready(),
                gate.reason().is_none(),
                "gate {gate:?} reported ready and blocked inconsistently"
            );
        }
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
