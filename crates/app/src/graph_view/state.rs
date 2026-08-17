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

/// An open branch-row right-click context menu in the Branches panel (GitHub issue #241): which
/// branch it targets, and the already-resolved window-space origin its popover paints at.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GraphBranchMenu {
    pub branch: String,
    pub origin_x: Pixels,
    pub origin_y: Pixels,
}

/// Which branch-name prompt is open, and what it will really do on Enter - see
/// [`GraphBranchPrompt`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GraphBranchPromptKind {
    /// The row menu's "Create branch here": `git checkout -b <typed name> <sha>`.
    CreateAt {
        sha: String,
        short_sha: String,
        subject: String,
    },
    /// The Branches panel's branch menu "Rename Branch…": `git branch -m <old_name> <typed
    /// name>`. `old_name` is captured at open time for the same reason the commit fields above
    /// are, and the field is pre-filled with it so a rename starts from the real current name.
    Rename { old_name: String },
}

/// GitHub issue #241: the graph tab's one branch-name prompt - `Some` only while the small,
/// hand-rolled modal (`crate::graph_view::render::AdeApp::render_graph_branch_prompt`) is open.
/// Mirrors `crate::root::new_file::NewFileInputState`'s own shape - this app's one established
/// "prompt for a name" idiom (append/backspace-only [`TextField`], Enter to confirm, Escape to
/// cancel) - rather than a second, competing one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GraphBranchPrompt {
    pub kind: GraphBranchPromptKind,
    /// A real rejection message from the last attempt - only this prompt's own "branch name
    /// can't be empty" guard (a real collision with an existing branch surfaces through
    /// [`GraphTabState::status_message`] instead, git's own real error text, exactly like every
    /// other menu mutation) - cleared on the very next keystroke.
    pub error: Option<String>,
}

/// Everything [`graph_branch_merge_gate`] is allowed to look at, already resolved into plain
/// values by its one producer (`crate::graph_view::render::AdeApp::graph_branch_merge_facts`).
/// Keeping the decision a pure function of these - rather than reaching into `AdeApp` from inside
/// it - is what makes every precondition below unit-testable without a GPUI window, the same split
/// `crate::rail::state::prunable_worktree_paths` already uses for the rail's own destructive
/// action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphBranchMergeFacts {
    /// The branch the merge would land *in*: the focused worktree's own checked-out branch.
    /// `None` on a detached `HEAD`, which `git merge` has no branch to merge into - the exact
    /// state `wt_core::merge::attempt_merge_into_current` refuses with `Error::MergeTargetDetached`.
    pub current_branch: Option<String>,
    /// The branch the merge would take commits *from*: the one whose row the menu was opened on.
    pub source_branch: String,
    /// How many real uncommitted paths the focused worktree has (`wt_core::stage::dirty_paths`,
    /// via `AdeApp::dirty_files`).
    pub uncommitted_files: usize,
    /// How many real agent sessions in the focused worktree are still `Run`ning or `Ask`ing.
    pub live_agents: usize,
    /// Whether the focused worktree has any agent tab at all for the existing conflict resolver to
    /// render in - see [`GraphBranchMergeGate::Blocked`]'s own docs and
    /// `crate::merge::flow::AdeApp::start_merge_from_graph_branch`.
    pub has_agent_tab: bool,
    /// `AdeApp::merge_flow.is_some()` - only one merge runs at a time app-wide.
    pub merge_already_running: bool,
}

/// Whether the branch menu's "Merge into current branch…" row is live, and if not, the real
/// reason - rendered *on the row itself*, dimmed and un-clickable, never a silent no-op (GitHub
/// issue #241). The design's rule: "disabled while anything is uncommitted, with the reason on
/// the button itself".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphBranchMergeGate {
    /// Every precondition holds. `current_branch` is the branch the merge would land in - the
    /// row's own sub-label when it is live, so the row always names its real target.
    Ready { current_branch: String },
    /// A real, user-facing reason this merge cannot run right now.
    Blocked(String),
}

impl GraphBranchMergeGate {
    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, GraphBranchMergeGate::Ready { .. })
    }

    /// The reason string for a blocked gate - `None` when ready, so a caller can never render a
    /// reason next to a live control.
    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            GraphBranchMergeGate::Ready { .. } => None,
            GraphBranchMergeGate::Blocked(reason) => Some(reason.as_str()),
        }
    }

    /// The row's sub-label: the real target branch while live, the real reason while blocked.
    /// One function so the two can never be rendered together or swapped by accident.
    pub(crate) fn sub_label(&self) -> String {
        match self {
            GraphBranchMergeGate::Ready { current_branch } => current_branch.clone(),
            GraphBranchMergeGate::Blocked(reason) => reason.clone(),
        }
    }
}

/// The one place the graph decides whether "Merge into current branch…" may run (GitHub issue
/// #241).
pub(crate) fn graph_branch_merge_gate(facts: &GraphBranchMergeFacts) -> GraphBranchMergeGate {
    let Some(current_branch) = facts.current_branch.clone() else {
        return GraphBranchMergeGate::Blocked(
            "detached HEAD \u{2013} no current branch".to_string(),
        );
    };
    if current_branch == facts.source_branch {
        return GraphBranchMergeGate::Blocked(format!("already on {current_branch}"));
    }
    if facts.merge_already_running {
        return GraphBranchMergeGate::Blocked("a merge is already running".to_string());
    }
    if facts.uncommitted_files > 0 {
        return GraphBranchMergeGate::Blocked(format!(
            "{} still uncommitted",
            plural::count(facts.uncommitted_files, "file", None)
        ));
    }
    if facts.live_agents > 0 {
        return GraphBranchMergeGate::Blocked(format!(
            "{} still working",
            plural::count(facts.live_agents, "agent", None)
        ));
    }
    if !facts.has_agent_tab {
        // The conflict resolver this action hands off to renders inside an agent's own work
        // surface (`crate::merge::render::AdeApp::render_merge_flow_surface` takes an `&Agent`,
        // reached only from `crate::work_surface::render`'s active-agent branch), so a worktree
        // with no agent tab at all has nowhere to show a conflicted merge. Refusing up front is
        // the honest reading of that: the alternative is starting a merge whose conflicts would be
        // invisible.
        return GraphBranchMergeGate::Blocked("no agent tab to show the merge".to_string());
    }
    GraphBranchMergeGate::Ready { current_branch }
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
    /// GitHub issue #241: which Branches-panel branch row's right-click context menu is open, if
    /// any, and the real position it was opened at - see [`GraphBranchMenu`]'s own docs for why
    /// this is keyed by branch name rather than by row index.
    pub branch_menu_open: Option<GraphBranchMenu>,
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
    /// GitHub issue #241: `Some(branch)` for exactly one real click past the two-click
    /// confirmation on the branch menu's "Delete Branch…" row, naming the branch that click
    /// targeted. The exact twin of [`Self::hard_reset_confirm_armed`] (see that field's own docs
    /// for the discipline): the *first* click on a given branch's Delete row only arms this and
    /// re-labels the row, without deleting anything; only a second click on that *same* branch's
    /// Delete row - with nothing else clicked in between - actually runs
    /// [`wt_core::checkout::delete_branch`]. Keyed by branch name rather than a bare `bool` for
    /// the identical reason: a Delete click on a *different* branch must arm its own
    /// confirmation, never ride on a stale arm left over from another row.
    pub delete_branch_confirm_armed: Option<String>,
    /// GitHub issue #241: the graph tab's one branch-name prompt, shared by the row menu's
    /// "Create branch here" and the branch menu's "Rename Branch…" - see [`GraphBranchPrompt`]'s
    /// own docs.
    pub branch_prompt: Option<GraphBranchPrompt>,
    /// The prompt's own real text input - a real undo history (GitHub issue #17), the same shape
    /// as [`Self::branches_filter`]. Lives independently of [`Self::branch_prompt`] (rather than
    /// nested inside it) only so it can be replaced wholesale (empty for a create, seeded with
    /// the current name for a rename) each time the prompt opens without reconstructing the whole
    /// prompt struct around it.
    pub branch_prompt_name: TextField,
    pub branch_prompt_focus_handle: FocusHandle,
    /// The currently in-flight remote operation's own real task - held so it isn't dropped (and
    /// therefore cancelled) the instant this function returns, matching
    /// `AdeApp::_worktree_history_task`'s identical one-slot-per-feature pattern. `None` when
    /// [`Self::remote_op_in_flight`] is `false`.
    pub _remote_op_task: Option<Task<()>>,
    /// GitHub issue #241: the branch menu's "Rebase current branch on Branch…" resolves that
    /// branch's real tip commit on the background executor before entering rebase mode
    /// (`crate::graph_view::rebase::AdeApp::enter_rebase_mode_onto_branch`) - held in its own slot
    /// rather than sharing [`Self::_remote_op_task`], since dropping that field's task would
    /// cancel a Fetch/Pull/Push genuinely still running, a completely unrelated operation.
    pub _branch_resolve_task: Option<Task<()>>,
    /// GitHub issue #221 ("Git graph only displays 500 commits"). The `max_commits` cap the
    /// currently loaded [`Graph`] was really walked with - `wt_core::graph::DEFAULT_MAX_COMMITS`
    /// after a fresh [`AdeApp::load_graph`], then one
    /// `crate::graph_view::render::LOAD_MORE_BATCH` higher per completed "load more".
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
            branch_menu_open: None,
            push_menu_open: false,
            push_button_bounds: Bounds::default(),
            branches_filter: TextField::new(),
            branches_filter_focus_handle: cx.focus_handle(),
            upstream_counts: None,
            status_message: None,
            remote_op_in_flight: false,
            push_force_confirm_armed: None,
            hard_reset_confirm_armed: None,
            delete_branch_confirm_armed: None,
            branch_prompt: None,
            branch_prompt_name: TextField::new(),
            branch_prompt_focus_handle: cx.focus_handle(),
            _remote_op_task: None,
            _branch_resolve_task: None,
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

    /// A fully-mergeable set of facts - every test below changes exactly one field, so a failure
    /// names the one precondition it is about rather than a whole hand-built struct.
    fn ready_merge_facts() -> GraphBranchMergeFacts {
        GraphBranchMergeFacts {
            current_branch: Some("main".to_string()),
            source_branch: "feature".to_string(),
            uncommitted_files: 0,
            live_agents: 0,
            has_agent_tab: true,
            merge_already_running: false,
        }
    }

    #[test]
    fn merge_gate_is_ready_and_names_the_branch_it_would_merge_into() {
        let gate = graph_branch_merge_gate(&ready_merge_facts());
        assert_eq!(
            gate,
            GraphBranchMergeGate::Ready {
                current_branch: "main".to_string()
            }
        );
        assert!(gate.is_ready());
        assert_eq!(
            gate.reason(),
            None,
            "a live control must never carry a blocked reason"
        );
        assert_eq!(
            gate.sub_label(),
            "main",
            "the live row's sub-label names its real target branch"
        );
    }

    #[test]
    fn merge_gate_blocks_a_detached_head_because_there_is_no_branch_to_merge_into() {
        let facts = GraphBranchMergeFacts {
            current_branch: None,
            ..ready_merge_facts()
        };
        let gate = graph_branch_merge_gate(&facts);
        assert!(!gate.is_ready());
        assert_eq!(
            gate.reason(),
            Some("detached HEAD \u{2013} no current branch")
        );
        assert_eq!(gate.sub_label(), "detached HEAD \u{2013} no current branch");
    }

    #[test]
    fn merge_gate_blocks_merging_a_branch_into_itself() {
        let facts = GraphBranchMergeFacts {
            source_branch: "main".to_string(),
            ..ready_merge_facts()
        };
        assert_eq!(
            graph_branch_merge_gate(&facts).reason(),
            Some("already on main")
        );
    }

    #[test]
    fn merge_gate_blocks_while_another_merge_is_already_running() {
        let facts = GraphBranchMergeFacts {
            merge_already_running: true,
            ..ready_merge_facts()
        };
        assert_eq!(
            graph_branch_merge_gate(&facts).reason(),
            Some("a merge is already running")
        );
    }

    #[test]
    fn merge_gate_blocks_on_uncommitted_files_in_the_designs_own_wording() {
        let one = GraphBranchMergeFacts {
            uncommitted_files: 1,
            ..ready_merge_facts()
        };
        assert_eq!(
            graph_branch_merge_gate(&one).reason(),
            Some("1 file still uncommitted")
        );
        let many = GraphBranchMergeFacts {
            uncommitted_files: 3,
            ..ready_merge_facts()
        };
        assert_eq!(
            graph_branch_merge_gate(&many).reason(),
            Some("3 files still uncommitted")
        );
    }

    #[test]
    fn merge_gate_blocks_on_live_agents_in_the_designs_own_wording() {
        let one = GraphBranchMergeFacts {
            live_agents: 1,
            ..ready_merge_facts()
        };
        assert_eq!(
            graph_branch_merge_gate(&one).reason(),
            Some("1 agent still working")
        );
        let many = GraphBranchMergeFacts {
            live_agents: 2,
            ..ready_merge_facts()
        };
        assert_eq!(
            graph_branch_merge_gate(&many).reason(),
            Some("2 agents still working")
        );
    }

    #[test]
    fn merge_gate_blocks_when_there_is_no_agent_tab_to_show_a_conflict_in() {
        let facts = GraphBranchMergeFacts {
            has_agent_tab: false,
            ..ready_merge_facts()
        };
        assert_eq!(
            graph_branch_merge_gate(&facts).reason(),
            Some("no agent tab to show the merge")
        );
    }

    #[test]
    fn merge_gate_reports_the_structural_refusal_first_when_several_hold_at_once() {
        // A detached HEAD with uncommitted files must not claim that committing them unblocks the
        // merge - it does not.
        let facts = GraphBranchMergeFacts {
            current_branch: None,
            uncommitted_files: 3,
            live_agents: 2,
            has_agent_tab: false,
            merge_already_running: true,
            ..ready_merge_facts()
        };
        assert_eq!(
            graph_branch_merge_gate(&facts).reason(),
            Some("detached HEAD \u{2013} no current branch")
        );

        // Uncommitted files outrank live agents: committing is the first thing to do, and the
        // agents may well be what is about to commit them.
        let dirty_and_busy = GraphBranchMergeFacts {
            uncommitted_files: 1,
            live_agents: 1,
            ..ready_merge_facts()
        };
        assert_eq!(
            graph_branch_merge_gate(&dirty_and_busy).reason(),
            Some("1 file still uncommitted")
        );
    }

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
