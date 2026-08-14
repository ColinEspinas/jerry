use super::*;
use crate::root::plural;
use crate::root::widgets::{
    hover_keycap_row, menu_popover_chrome, render_env_chip, render_keycap_row, text_tooltip,
    KeycapSize,
};
use crate::status_bar::resources::{
    self, LoadLevel, ResourceGroup, ResourceRow, ResourceTree, JERRY_GROUP_LABEL,
};

/// This machine's real core count, read once and cached.
///
/// `std::thread::available_parallelism` is not the cheap `sched_getaffinity` call it looks like:
/// on Linux it also performs the cgroup quota lookup, opening and reading `/proc/self/cgroup`,
/// `/proc/self/mountinfo` and the cgroup's `cpu.max`/`cpu.cfs_quota_us` files. Calling it from
/// the per-frame resource-tree build meant real blocking filesystem I/O on the UI thread on
/// *every frame*, for a value that cannot change while the process is running. This is one
/// `OnceLock` instead - and deliberately not a field on `AdeApp`, since it is a property of the
/// machine rather than of any window.
fn available_cores() -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CORES.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

/// The three type tiers rev 6 rebuilt the bar around
/// (`design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §3, and
/// `STAGE-A-CHANGELOG.md` §4c's diagnosis: "the cause was not density - it was that after the
/// cut, **all thirteen-then-five readouts sat at 10px in `#4a5057`**, one flat tone on `#101214`.
/// No hierarchy means the eye has to read all of it to find any of it").
///
/// A named enum rather than three ad-hoc `.text_color(...)`/`.text_size(...)` pairs at each call
/// site: the whole defect was that every readout independently picked the same tone, so the fix
/// has to be a closed set of tiers a readout is *assigned to*, not a colour it chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusTier {
    /// §3: `main ↑2 ↓0`'s branch, `4 agents running` - "the readouts you are meant to find
    /// first".
    Primary,
    /// §3: provider budgets. Nothing in the bar carries this tier until GitHub issue #294 lands
    /// its per-provider rate-limit clusters; the token itself is live today in the Resources
    /// popover's memory column (see `crate::theme::status_bar::SECONDARY`).
    Secondary,
    /// §3: `41% cpu · 3.4 GB`, and the tail of every split readout (`↑2 ↓0`).
    Recessive,
}

impl StatusTier {
    fn color(self) -> theme::ColorToken {
        match self {
            StatusTier::Primary => theme::status_bar::PRIMARY,
            StatusTier::Secondary => theme::status_bar::SECONDARY,
            StatusTier::Recessive => theme::status_bar::RECESSIVE,
        }
    }

    /// §4c's per-tier sizes: `10.5px/450` primary, `10.5px` secondary, `9.5-10.5px` tertiary.
    fn text_size(self) -> f32 {
        match self {
            StatusTier::Primary | StatusTier::Secondary => 10.5,
            StatusTier::Recessive => 10.0,
        }
    }

    fn weight(self) -> gpui::FontWeight {
        match self {
            StatusTier::Primary => gpui::FontWeight::MEDIUM,
            StatusTier::Secondary | StatusTier::Recessive => gpui::FontWeight::NORMAL,
        }
    }
}

/// A composite readout split into §4c's "bright head and dim tail" - "the count is the fact you
/// scan for, the size is the detail you read only if the count surprised you".
///
/// Pure, and returned rather than rendered directly, so which half is which is a testable fact
/// rather than a property of a `div` tree.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SplitReadout {
    /// Rendered at [`StatusTier::Primary`].
    pub(crate) head: String,
    /// Rendered at [`StatusTier::Recessive`]. `None` when there genuinely is no tail yet - an
    /// ahead/behind that hasn't been computed is honestly omitted rather than shown as a
    /// fabricated `↑0 ↓0`.
    pub(crate) tail: Option<String>,
}

/// The branch cluster's split: the branch name is the fact you scan for, the ahead/behind counts
/// are the detail. §3's own example (`main ↑2 ↓0`) is exactly this pair, and its table puts the
/// two halves in two different tiers.
fn branch_readout(branch: String, ahead_behind: Option<(usize, usize)>) -> SplitReadout {
    SplitReadout {
        head: branch,
        tail: ahead_behind.map(|(ahead, behind)| format!("\u{2191}{ahead} \u{2193}{behind}")),
    }
}

/// `"4 agents running"` - §4b's `footRun`, the running-agent count on its own after the old
/// `4 agents · 41% cpu · 3.4 GB` composite was split ("`footAgents` split into `footRun` +
/// `footLoad`"). Conjugated through [`plural`], never an inline ternary (rev 6 §7 rule 9).
fn running_agents_label(count: usize) -> String {
    format!("{} running", plural::count(count, "agent", None))
}

/// The 30px status bar, rebuilt for rev 6 (GitHub issue #293).
///
/// ## What this bar carries, and what it deliberately no longer does
///
/// `design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4b counted the old bar's thirteen
/// readouts and found eight of them lifted from VS Code's status bar: "VS Code's footer answers
/// 'what am I typing into'; Jerry's job is watching agents. Wrong app's chrome." Deleted with
/// this rebuild, code paths and all (§7 rule 5: "Replacing a control means deleting its old keys
/// in the same edit"): `ln N`, the indent width, the line ending, the encoding, the editor-zoom
/// readout, the UI-scale readout, `N servers · M errors`, the five urgency-counter dots, and the
/// `N wt · Y GB` cluster.
///
/// - **Editor zoom** survives as `mod+plus`/`mod+minus` only (§4b's table: "keyboard only
///   (`⌘+`/`⌘−`); state and handlers kept, both controls gone"). `AdeApp::zoom_in`/`zoom_out`/
///   `reset_zoom` and `settings.appearance.editor_zoom_percent` are all untouched - only the two
///   *readouts* are gone.
/// - **The urgency-counter dots** are §4b's "footer dot cluster", folded into the title bar's own
///   compact dot chips (`crate::title_bar::render`). §7 rule 4 - "Two states distinguished
///   anywhere in the app are never summed anywhere in it" - is why the bar's remaining agent
///   readout counts one state (`running`) rather than every open agent.
/// - **`N wt · Y GB`** is §4d's one removed duplicate: "the rail owns worktree inventory and its
///   prune action, the bar owns activity and cost". The rail footer carries it 30px away.
/// - **`N servers · M errors`** is §4b's "diagnostics count ... strip badge only".
///
/// What is left is three visible groups, each on its own tier, separated by §4c's heavier 13-high
/// divider (`theme::status_bar::DIVIDER`; "at `#22262a` they were invisible, so the groups they
/// were meant to separate ran together").
impl AdeApp {
    pub(crate) fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("status-bar")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(px(13.0))
            .w_full()
            .h(theme::band::STATUS_BAR)
            .bg(theme::surface::TITLE_BAR)
            .border_t_1()
            .border_color(theme::border::ZONE)
            .child(self.render_status_bar_left(cx))
            .child(self.render_status_bar_right(cx))
    }

    /// Branch cluster · `N agents running` · `X% cpu · Y GB`, separated by §4c's real dividers.
    fn render_status_bar_left(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut segments: Vec<gpui::AnyElement> = Vec::new();
        // Shown first - persistent, actionable "a real update is available/ready" information
        // (GitHub issue #87, `crate::updater`) outranks the worktree-history notice just below,
        // which is only ever a one-off, self-clearing completion message. Renders nothing at all
        // (`None`) unless there's genuinely something to say - see
        // `crate::updater::render::AdeApp::render_status_update_notice`'s own docs.
        if let Some(update_notice) = self.render_status_update_notice(cx) {
            segments.push(update_notice);
        }
        // Shown next, and only while genuinely set - the real, transient feedback from
        // "keep all changes"/"discard worktree" (Revision R10). See this method's own docs for
        // why this lives here, in the status bar, rather than the rail footer's own status slot.
        if let Some(notice) = self.render_status_worktree_history_notice() {
            segments.push(notice);
        }
        if let Some(branch_cluster) = self.render_status_branch_cluster(cx) {
            segments.push(branch_cluster);
        }
        segments.push(self.render_status_running_agents(cx).into_any_element());
        if let Some(resources) = self.render_status_resources_readout(cx) {
            segments.push(resources);
        }
        render_status_segment_row(segments).into_any_element()
    }

    /// The transient "keeping all changes…"/"discarded …" feedback from
    /// `AdeApp::keep_all_changes`/`AdeApp::execute_discard_worktree` (`worktree_history::flow`,
    /// Revision R10) - `None` (nothing rendered at all) whenever
    /// [`AdeApp::worktree_history_status`] is `None`.
    ///
    /// Deliberately shown here, in the status bar, rather than the rail footer's own status slot
    /// (`Self::render_rail_footer`) - an audit found two real problems with that shared slot:
    /// [`AdeApp::prune_status`] took priority there and is never cleared once set (see that
    /// field's own docs), so a single prune click permanently hid every future worktree-history
    /// status for the rest of the agent; and the rail footer disappears entirely while Settings
    /// is open (`AdeApp::render_workspace_body` isn't called - `root/mod.rs`'s `Render` impl
    /// swaps it out for `Self::render_settings`), leaving *no* real status surface at all for
    /// this feedback while Settings is open. The status bar is rendered as an unconditional
    /// sibling of that swap, so it stays visible either way.
    ///
    /// Long, load-bearing text (`Error::DiscardRemovalFailedAfterStash`'s real stash id,
    /// `Error::HeadMovedSinceRecorded`'s two full 40-character commit shas, ...) is truncated
    /// with an ellipsis and carries a real tooltip ([`text_tooltip`]) with the untruncated text -
    /// an audit found this exact text rendered with no truncation or tooltip at all before.
    ///
    /// Rendered at [`StatusTier::Primary`]: a notice only exists when there is genuinely
    /// something to tell you, which is by definition the thing to read first.
    fn render_status_worktree_history_notice(&self) -> Option<gpui::AnyElement> {
        let status = self.worktree_history_status.clone()?;
        Some(
            div()
                .id("status-bar-worktree-history-notice")
                .min_w_0()
                .max_w(px(320.0))
                .truncate()
                .tooltip(text_tooltip(status.clone()))
                .child(self.render_status_tier_text(status, StatusTier::Primary))
                .into_any_element(),
        )
    }

    /// The environment chip and the palette/agent keycap hints - all that is left on the right
    /// after §4b's subtractive pass (the file/editor cluster and the LSP counts were the rest of
    /// it, and both are gone).
    fn render_status_bar_right(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let hints = div()
            .flex()
            .items_center()
            .gap(px(16.0))
            .child(self.render_status_palette_hint(cx))
            .child(self.render_status_agent_hint());

        render_status_segment_row(vec![
            render_env_chip().into_any_element(),
            hints.into_any_element(),
        ])
        .into_any_element()
    }

    /// The current worktree's real branch name (`self.worktrees`, the same lookup
    /// `Self::render_agent_context_bar` already does - not a second copy) plus its real
    /// `↑ahead ↓behind` from [`Self::ahead_behind_cache`] (populated by the same periodic
    /// `wt_core::diff::ahead_behind_against_base` refresh as [`Self::diff_cache`]). A detached
    /// `HEAD` still shows real text (`"(detached)"`), matching the agent context bar's own
    /// convention, and a not-yet-computed ahead/behind is honestly omitted rather than shown as
    /// a fabricated `↑0 ↓0`.
    ///
    /// Split across two tiers per §4c - see [`branch_readout`]. The cluster as a whole is a real
    /// click target that opens the graph tab via
    /// `crate::graph_view::render::AdeApp::open_git_graph`, the same entry point the `+` menu and
    /// the palette's "Open git graph" use.
    ///
    /// ## Why this is gated on [`Self::focused_repo`], not on an active agent
    ///
    /// It used to open with `self.agents.active()?`, and that `?` was **not** a deliberate
    /// visibility rule - it was how the branch got a `cwd` to look itself up by. When this
    /// function was written (Revision R6) the app had no `repos` and no [`Self::focused_repo`]
    /// at all, so the active pane's `cwd` was the only path available, and hiding the row when
    /// there was no pane was an incidental side effect nobody was designing for. The git graph
    /// tab then hung its click target, fork glyph and
    /// `crate::graph_view::render::AdeApp::open_git_graph` call off this same function, which
    /// silently promoted that leftover `?` into the visibility policy for the graph's primary
    /// entry point.
    ///
    /// The result was a real bug: `open_git_graph` refuses only when
    /// [`Self::focused_repo`] is `None`, so with a repo focused and simply no agent open (every
    /// tab closed) the action worked perfectly while its only button was hidden - and the whole
    /// cluster (fork glyph, branch, `↑ahead ↓behind`) vanished with it. The gate is now the
    /// action's own precondition, so the button is visible exactly when clicking it does
    /// something, and the path comes from [`Self::current_worktree_path`] - the app's real current
    /// git context, which already resolves the selected worktree and falls back to
    /// [`Self::focused_repo_path`] - rather than from whichever pane happens to be focused.
    fn render_status_branch_cluster(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        // Exactly `open_git_graph`'s own guard: this cluster is that action's button, so the two
        // must not be able to disagree about when it is available. Unlike the `self.agents
        // .active()?` this replaces, the value is genuinely unused - the `?` *is* the gate here,
        // not an incidental by-product of fetching a `cwd`.
        self.focused_repo()?;
        // `current_worktree_path` is `None` only in the brief in-flight window before the worktree
        // fetch lands, or the genuine no-usable-worktree error state - both real, but neither is
        // a reason to hide this cluster once a repo is focused at all, so this falls back to the
        // repo root exactly as `Self::checkout_repo_from_rail`'s synchronous seed does.
        let cwd = self
            .current_worktree_path()
            .unwrap_or_else(|| self.focused_repo_path());
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.path == cwd)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| "(detached)".to_string());
        let readout = branch_readout(
            branch,
            self.ahead_behind_cache
                .get(&cwd)
                .map(|ahead_behind| (ahead_behind.ahead, ahead_behind.behind)),
        );

        let mut row = div()
            .id("status-bar-branch-cluster")
            .debug_selector(|| "status-bar-branch-cluster".to_string())
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .rounded(theme::radius::CHIP)
            .px(px(5.0))
            .h(px(18.0))
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.open_git_graph(window, cx);
            }))
            .child(crate::graph_view::render::render_graph_tab_chip())
            .child(self.render_status_tier_text(readout.head, StatusTier::Primary));

        if let Some(tail) = readout.tail {
            row = row.child(self.render_status_tier_text(tail, StatusTier::Recessive));
        }

        Some(row.into_any_element())
    }

    /// §4b's `footRun`: `"4 agents running"`, the real count of agents whose derived
    /// [`Status`] is [`Status::Run`] right now.
    ///
    /// Counting one state rather than summing every open agent is §7 rule 4 - "Two states
    /// distinguished anywhere in the app are never summed anywhere in it". The title bar
    /// distinguishes `ask`/`fail`/`run`, so a bar readout that added them back together would be
    /// the exact restatement that rule forbids. The count comes from [`rail::urgency_counts`]
    /// over [`Self::build_agent_rows`] - the same real per-agent classification the rail and the
    /// title bar's own chips use, so the three can never disagree.
    fn render_status_running_agents(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.build_agent_rows(cx);
        let running = rail::urgency_counts(&rows)
            .into_iter()
            .find(|(status, _)| *status == Status::Run)
            .map(|(_, count)| count)
            .unwrap_or(0);
        div()
            .id("status-bar-running-agents")
            .debug_selector(|| "status-bar-running-agents".to_string())
            .child(self.render_status_tier_text(running_agents_label(running), StatusTier::Primary))
    }

    /// §4d's one resource readout: `41% cpu · 3.4 GB`, clickable, opening the Resources popover.
    ///
    /// **The number is the sum of that popover's own tree** - see
    /// [`crate::status_bar::resources`]. Nothing here aggregates a second time.
    ///
    /// **No meter here**, verbatim from §4d: "the budget meter beside it fills with *headroom* -
    /// full is good. A load meter fills with *usage* - full is bad. Two meters 40px apart meaning
    /// opposite things is the exact incoherence this pass keeps removing." The load meters live
    /// inside the popover, under `CPU`/`MEMORY` labels that make the direction unambiguous.
    ///
    /// The text takes the load hue only once the machine is genuinely strained
    /// ([`resources::load_level`]); a healthy load stays [`StatusTier::Recessive`] and spends no
    /// attention colour at all.
    ///
    /// `None` - no readout, and so no popover trigger - on a build with no real sampling backend
    /// at all (`process_stats::PLATFORM_SAMPLING_SUPPORTED`, which today means FreeBSD). A
    /// permanent `...% cpu` that can never resolve is the one placeholder that must not reach the
    /// screen; shipping `…` off-Linux was explicitly rejected for issue #293's own dependency
    /// (#283), and this is the same rule for the platform where there really is nothing to show.
    fn render_status_resources_readout(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !process_stats::PLATFORM_SAMPLING_SUPPORTED {
            return None;
        }
        let tree = self.build_resource_tree(cx);
        let readout = tree.bar_readout();
        let level = resources::load_level(tree.cpu_percent());
        let color = if level == LoadLevel::Neutral {
            StatusTier::Recessive.color()
        } else {
            level.color()
        };

        Some(
            div()
                .id("status-bar-resources")
                .debug_selector(|| "status-bar-resources".to_string())
                .flex()
                .items_center()
                .h(px(18.0))
                .px(px(5.0))
                .rounded(theme::radius::CHIP)
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .when(self.resources_popover_open, |el| {
                    el.bg(theme::surface::ROW_HOVER_ALT)
                })
                .tooltip(text_tooltip(
                    "What Jerry is costing this machine right now".to_string(),
                ))
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .font_weight(StatusTier::Recessive.weight())
                        .text_size(self.ui_text_size(StatusTier::Recessive.text_size()))
                        .text_color(color)
                        .child(readout),
                )
                .child({
                    let this = cx.entity();
                    gpui::canvas(
                        move |bounds, _window, cx| {
                            this.update(cx, |this, _cx| {
                                this.resources_readout_bounds = bounds;
                            });
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full()
                })
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    let opening = !this.resources_popover_open;
                    // GitHub issue #176's shared invariant: opening this popover closes whatever
                    // else was open. Read before the sweep and applied after it, because the
                    // sweep clears `resources_popover_open` itself.
                    let _ = this.close_menu_surfaces_except(Some(menus::MenuSurface::Resources));
                    this.resources_popover_open = opening;
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// The real `repo → worktree → agent` cost tree behind both the bar readout and the popover -
    /// §4d: "the same hierarchy as the rail".
    ///
    /// Built by walking [`Self::build_repo_groups`]'s already-ranked
    /// `RepoGroup → WorktreeRow → AgentRow` structure rather than re-deriving a second grouping:
    /// the rail's grouping *is* the hierarchy this popover is specified in terms of, so a second
    /// implementation of it could only ever drift from the rail it is supposed to mirror.
    ///
    /// [`rail::RepoGroup::all_rows`], not `rows`: the rail's filter box narrows what the rail
    /// *displays*, and a resource total that shrank as you typed in an unrelated text field would
    /// be a lie about what the machine is doing.
    ///
    /// Jerry's own process is a real row of its own (`JERRY_GROUP_LABEL`), because the readout's
    /// tooltip promises "what Jerry is costing this machine right now" and a total that excluded
    /// the window, its editors and its language servers would not be that number.
    ///
    /// Known, accepted redundancy: this calls `build_repo_groups`, which the rail already calls
    /// once in the same frame - the same shape (and the same reason) as the redundant
    /// `build_agent_rows` call the title bar's chips already make. Both are `&self` renders, so
    /// caching a frame's result on `self` would need either widened `&mut self` signatures across
    /// several rail methods or interior mutability; neither is worth it for a computation that
    /// does no I/O.
    pub(crate) fn build_resource_tree(&self, cx: &mut Context<Self>) -> ResourceTree {
        let cores = available_cores();
        let mut rows: Vec<ResourceRow> = Vec::new();
        let mut attributed: std::collections::HashSet<crate::work_surface::agents::AgentId> =
            std::collections::HashSet::new();

        // Walked in the rail's own order - repo groups ranked by their most urgent worktree,
        // worktrees ranked inside them - so the popover's tree reads top to bottom the way the
        // rail beside it does.
        for group in self.build_repo_groups(cx) {
            for worktree in &group.all_rows {
                let worktree_label = worktree
                    .branch
                    .clone()
                    .unwrap_or_else(|| worktree.label.clone());
                // Matched against `Self::agents` directly rather than against
                // `WorktreeRow::agents`: that list is `build_agent_rows`, which filters to
                // `ProcessKind::is_agent_session()` and so excludes every shell - and a shell is
                // a real process burning real CPU in a real worktree. The rail is right to leave
                // shells out of an *agent* list; a cost breakdown that left them out would
                // under-report, and the bar readout above it claims to be the whole cost.
                for agent in self.agents.iter().filter(|open| open.cwd == worktree.path) {
                    let Some(pid) = agent.pane.read(cx).pid() else {
                        // A pane with no pid has genuinely exited (`TerminalPane::pid` returns
                        // `None` once its agent is cleared), so there is nothing to attribute.
                        continue;
                    };
                    attributed.insert(agent.id);
                    let (cpu_percent, memory_bytes) =
                        resources::row_sample(pid, &self.process_stats, cores);
                    rows.push(ResourceRow {
                        repo_name: group.repo_name.clone(),
                        agent_label: agent.kind.label().to_string(),
                        worktree_label: worktree_label.clone(),
                        kind: Some(agent.kind),
                        pid,
                        cpu_percent,
                        memory_bytes,
                    });
                }
            }
        }

        // Anything the rail's hierarchy does not currently account for still costs this machine
        // something, and the readout claims to be the whole cost. An agent is genuinely missing
        // from that hierarchy whenever its cwd is not (yet) in any repo's worktree list: a repo
        // whose `wt_core::list_worktrees` fetch has not landed, a directory that is not a git
        // repository at all (the startup shell still runs there and still burns CPU), or a
        // worktree removed from under a still-running agent. Dropping those rows would make the
        // bar quietly under-report - the same class of defect as a hardcoded total, just in the
        // other direction - so they are attributed to their own repo by path instead, and the
        // hierarchy stays complete.
        for agent in self.agents.iter() {
            if attributed.contains(&agent.id) {
                continue;
            }
            let Some(pid) = agent.pane.read(cx).pid() else {
                continue;
            };
            let repo_name = self
                .repos
                .iter()
                .filter(|repo| agent.cwd.starts_with(&repo.path))
                // The longest matching prefix, so a repo nested inside another is attributed to
                // itself rather than to its ancestor.
                .max_by_key(|repo| repo.path.as_os_str().len())
                .map(|repo| repo.name.clone())
                .unwrap_or_else(|| JERRY_GROUP_LABEL.to_string());
            let (cpu_percent, memory_bytes) =
                resources::row_sample(pid, &self.process_stats, cores);
            rows.push(ResourceRow {
                repo_name,
                agent_label: agent.kind.label().to_string(),
                worktree_label: agent
                    .cwd
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| agent.cwd.to_string_lossy().to_string()),
                kind: Some(agent.kind),
                pid,
                cpu_percent,
                memory_bytes,
            });
        }

        let own_pid = std::process::id();
        let (cpu_percent, memory_bytes) =
            resources::row_sample(own_pid, &self.process_stats, cores);
        rows.push(ResourceRow {
            repo_name: JERRY_GROUP_LABEL.to_string(),
            agent_label: "Jerry".to_string(),
            worktree_label: "window, editors, LSP".to_string(),
            kind: None,
            pid: own_pid,
            cpu_percent,
            memory_bytes,
        });

        ResourceTree::from_rows(rows)
    }

    /// The `⌘P commands` hint: clicking it (or pressing the bound `secondary-p` - see
    /// [`TogglePalette`]) opens the command palette.
    fn render_status_palette_hint(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // GitHub issue #128.
        hover_keycap_row(div().id("status-bar-open-palette").cursor_pointer())
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(render_keycap_row(
                &keymap::resolve_combo("mod+P", self.window_controls_style().is_macos()),
                KeycapSize::Standard,
            ))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::FAINT)
                    .child("commands"),
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.open_palette(window, cx);
            }))
    }

    /// The real agent-jump keycap hint (`secondary-1`..`secondary-8`) - reuses
    /// [`Self::agent_jump_keys`], the exact same computation `Self::render_tab_strip`'s own
    /// right-aligned cluster uses, not a second copy that could drift from what's really bound.
    fn render_status_agent_hint(&self) -> impl IntoElement {
        let jump_keys = self.agent_jump_keys();
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(render_keycap_row(&jump_keys, KeycapSize::Standard))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::FAINT)
                    .child("agent"),
            )
    }

    /// One status-bar readout, rendered in its assigned [`StatusTier`] - the one place a bar
    /// readout gets its colour, size and weight, so no readout can quietly pick the flat tone
    /// §4c removed.
    fn render_status_tier_text(&self, label: String, tier: StatusTier) -> impl IntoElement {
        div()
            .font(font(theme::font::MONO))
            .font_weight(tier.weight())
            .text_size(self.ui_text_size(tier.text_size()))
            .text_color(tier.color())
            .child(label)
    }
}

/// Lays out already-built segments with a real 1px vertical divider between each consecutive
/// pair - segments that don't apply right now are simply never pushed into the `Vec` by the
/// caller, so no divider ever appears next to a missing field.
///
/// §4c: "group gap 9 -> 14, dividers `#22262a` -> `#2b3137` at 13 high".
fn render_status_segment_row(segments: Vec<gpui::AnyElement>) -> impl IntoElement {
    let mut row = div().flex().items_center().gap(px(14.0));
    for (index, segment) in segments.into_iter().enumerate() {
        if index > 0 {
            row = row.child(render_status_divider());
        }
        row = row.child(segment);
    }
    row
}

fn render_status_divider() -> impl IntoElement {
    div()
        .flex_none()
        .w(px(1.0))
        .h(px(13.0))
        .bg(theme::status_bar::DIVIDER)
}

/// The Resources popover (§4d) - 320 wide, above the bar, mirroring the rate-limit popover's own
/// chrome so the two read as one family.
impl AdeApp {
    /// The popover's real width (§4d: "The popover (320 wide, mirrors the rate-limit one)").
    const RESOURCES_POPOVER_WIDTH: f32 = 320.0;

    /// The whole overlay: a transparent click-away scrim plus the panel itself, positioned off
    /// the readout's real painted bounds ([`Self::resources_readout_bounds`]) - the same shape
    /// `Self::render_plus_menu` uses, and a direct child of the root element for the same reason
    /// (the captured bounds are window-space, so `.absolute()` positioning built from them is
    /// only correct there).
    pub(crate) fn render_resources_popover(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = self.build_resource_tree(cx);
        let anchor = self.resources_readout_bounds;
        let width = px(Self::RESOURCES_POPOVER_WIDTH);
        // The bar sits at the very bottom of the window, so the panel opens *upwards* from the
        // readout's own top edge rather than downwards from its bottom like every other menu in
        // the app. `left` is clamped to the window so a bar readout near the right edge can't
        // push the panel off screen.
        let left = anchor.origin.x - px(10.0);

        div()
            .id("status-bar-resources-scrim")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(crate::work_surface::state::TRANSPARENT)
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                // `stop_propagation` is what makes this a scrim rather than a transparent sheet:
                // without it the click also reaches whatever is underneath, and clicking the
                // readout itself to dismiss the popover would close it here and immediately
                // reopen it there (the readout's own toggle reads the flag *after* this ran) -
                // a popover that could not be closed by clicking the control that opened it.
                cx.stop_propagation();
                this.resources_popover_open = false;
                cx.notify();
            }))
            .child(
                menu_popover_chrome(
                    div()
                        .id("status-bar-resources-popover")
                        .debug_selector(|| "status-bar-resources-popover".to_string())
                        .absolute()
                        .left(left.max(px(6.0)))
                        .bottom(theme::band::STATUS_BAR + px(4.0))
                        .w(width)
                        .flex()
                        .flex_col(),
                    theme::shadow::MENU,
                )
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .child(self.render_resources_popover_header())
                .child(self.render_resources_headline_stats(&tree))
                .child(self.render_resources_live_tree(&tree))
                .child(self.render_resources_disk_line(cx))
                .child(self.render_resources_footer()),
            )
    }

    fn render_resources_popover_header(&self) -> impl IntoElement {
        div()
            .flex()
            .items_center()
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
                    .child("RESOURCES"),
            )
    }

    /// §4d's three headline stats, each under a label that makes the meter's direction
    /// unambiguous: `CPU` and `MEMORY` fill with *usage* (full is bad), which is exactly why they
    /// are here and not next to the budget meter in the bar.
    ///
    /// `ON DISK` deliberately carries **no meter**. A meter needs an honest denominator, and Jerry
    /// has no real one for disk: the mock's own `18%` fill is a literal, and a fill against a
    /// guessed total is the "hardcoded number that drifts from its own breakdown" §4d names as the
    /// defect this panel would otherwise ship with. The value itself is real
    /// ([`Self::disk_usage_label`], the same figure the rail footer shows).
    fn render_resources_headline_stats(&self, tree: &ResourceTree) -> impl IntoElement {
        let cpu = tree.cpu_percent();
        let memory = tree.memory_bytes();
        let total_memory = process_stats::system_memory_bytes();
        let memory_fraction = resources::meter_fraction(memory, total_memory);
        let memory_level = resources::load_level(memory_fraction.map(|f| f * 100.0));

        div()
            .flex()
            .px(px(11.0))
            .pt(px(9.0))
            .pb(px(10.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(self.render_resources_stat(
                "CPU",
                resources::cpu_label(cpu),
                cpu.map(|percent| percent / 100.0),
                resources::load_level(cpu),
            ))
            .child(self.render_resources_stat(
                "MEMORY",
                resources::memory_label(memory),
                memory_fraction,
                memory_level,
            ))
            .child(self.render_resources_stat(
                "ON DISK",
                self.disk_usage_label(),
                None,
                LoadLevel::Neutral,
            ))
    }

    /// One headline stat: label, value, and - when there is a real denominator to fill against -
    /// a 3px load meter in that load's own hue.
    fn render_resources_stat(
        &self,
        label: &'static str,
        value: String,
        fraction: Option<f32>,
        level: LoadLevel,
    ) -> impl IntoElement {
        let value_color = if level == LoadLevel::Neutral {
            theme::text::SELECTED
        } else {
            level.color()
        };
        div()
            .flex_1()
            .min_w_0()
            .pr(px(10.0))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(self.ui_text_size(9.0))
                    .text_color(theme::status_bar::SECTION_LABEL)
                    .child(label),
            )
            .child(
                div()
                    .pt(px(3.0))
                    .pb(px(5.0))
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(13.0))
                    .text_color(value_color)
                    .child(value),
            )
            // No track at all when there is no honest denominator - an empty track would read as
            // a real meter sitting at zero.
            .when_some(fraction, |el, fraction| {
                el.child(
                    div()
                        .h(px(3.0))
                        .w_full()
                        .rounded(px(2.0))
                        .bg(theme::status_bar::METER_TRACK)
                        .child(
                            div()
                                .h(px(3.0))
                                .w(gpui::relative(fraction))
                                .rounded(px(2.0))
                                .bg(level.color()),
                        ),
                )
            })
    }

    /// §4d's `LIVE NOW` tree: one section per repo, with a real per-repo subtotal, and one
    /// `tint · agent · worktree · cpu · memory` row per agent under it.
    fn render_resources_live_tree(&self, tree: &ResourceTree) -> impl IntoElement {
        div()
            .px(px(11.0))
            .pt(px(7.0))
            .pb(px(8.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .pb(px(4.0))
                    .child(
                        div()
                            .flex_1()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(self.ui_text_size(9.0))
                            .text_color(theme::status_bar::SECTION_LABEL)
                            .child("LIVE NOW"),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::text::HINT)
                            .child("cpu \u{b7} memory"),
                    ),
            )
            .child(
                // Capped and scrollable: the panel grows *upwards* from a bar at the very bottom
                // of the window, so an unbounded tree would push its own headline stats off the
                // top of the screen once enough agents were open. 168px is eight rows plus their
                // group headers - past that the list scrolls inside the panel instead.
                div()
                    .id("status-bar-resources-tree")
                    .max_h(px(168.0))
                    .overflow_y_scroll()
                    .children(
                        tree.groups
                            .iter()
                            .map(|group| self.render_resources_group(group)),
                    ),
            )
    }

    fn render_resources_group(&self, group: &ResourceGroup) -> impl IntoElement {
        let is_jerry = group.repo_name == JERRY_GROUP_LABEL;
        div()
            .pt(px(2.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(17.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(9.5))
                            .text_color(if is_jerry {
                                theme::status_bar::SECTION_LABEL
                            } else {
                                theme::rail::REPO_HEADER_NAME
                            })
                            .child(group.repo_name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::text::GHOSTER)
                            .child(group.subtotal_label()),
                    ),
            )
            .children(group.rows.iter().map(|row| self.render_resources_row(row)))
    }

    /// One agent's row. The tint chip is the agent's own
    /// `crate::work_surface::state::agent_tint` colour - the same mark the rail and the palette
    /// use for that agent, not a second colour vocabulary. Jerry's own row has no agent tint and
    /// deliberately does not borrow one.
    fn render_resources_row(&self, row: &ResourceRow) -> impl IntoElement {
        let tint = match row.kind {
            Some(kind) => crate::work_surface::state::agent_tint(kind).0,
            None => theme::text::GHOST.into(),
        };
        let cpu_level = resources::load_level(row.cpu_percent);
        let cpu_color = if cpu_level == LoadLevel::Neutral {
            theme::status_bar::PRIMARY
        } else {
            cpu_level.color()
        };

        div()
            .flex()
            .items_center()
            .gap(px(7.0))
            .h(px(20.0))
            .pl(px(9.0))
            .child(
                div()
                    .flex_none()
                    .w(px(5.0))
                    .h(px(5.0))
                    .rounded(px(1.0))
                    .bg(tint),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::STRONG)
                    .child(row.agent_label.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::FAINTER)
                    .child(row.worktree_label.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.0))
                    .text_color(cpu_color)
                    .child(resources::cpu_label(row.cpu_percent)),
            )
            .child(div().flex_none().child(self.render_status_tier_text(
                resources::memory_label(row.memory_bytes),
                StatusTier::Secondary,
            )))
    }

    /// §4d's disk line: `N worktrees prunable · X MB · Prune`.
    ///
    /// Every part of it is the rail footer's own real state, reached through the same methods:
    /// the candidate list is [`Self::prunable_worktree_paths`] (so what is shown always matches
    /// what a click will do), the size sums those candidates' own entries in
    /// [`Self::worktree_disk_usage`], and `Prune` calls [`Self::request_prune`] - the same
    /// two-click confirmation the rail footer's button uses, not a second, unconfirmed path to a
    /// destructive action.
    fn render_resources_disk_line(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let candidates = self.prunable_worktree_paths();
        let known_sizes: Vec<u64> = candidates
            .iter()
            .filter_map(|path| self.worktree_disk_usage.get(path))
            .map(|(bytes, _truncated)| *bytes)
            .collect();
        let prunable_bytes = resources::prunable_total_bytes(candidates.len(), &known_sizes);
        let armed = self.prune_confirm_armed;
        let enabled = !candidates.is_empty() && !self.prune_in_flight;

        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(11.0))
            .pt(px(7.0))
            .pb(px(8.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::STRONG)
                    .child(resources::prunable_label(candidates.len())),
            )
            .child(div().flex_none().child(self.render_status_tier_text(
                resources::memory_label(prunable_bytes),
                StatusTier::Secondary,
            )))
            .child({
                let button = div()
                    .id("status-bar-resources-prune")
                    .debug_selector(|| "status-bar-resources-prune".to_string())
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.0))
                    .child(if armed { "Confirm" } else { "Prune" });
                // Rev 6 §7 rule 2: "A control that acts on results does not exist when there are
                // none." With nothing to prune the label stays, disabled and inert, rather than
                // inviting a click `Self::execute_prune`'s own guard would silently swallow.
                if enabled {
                    button
                        .cursor_pointer()
                        .text_color(theme::button::BLUE_FG)
                        .hover(|el| el.text_color(theme::text::SELECTED))
                        .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.request_prune(cx);
                        }))
                } else {
                    button.cursor_default().text_color(theme::text::DISABLED)
                }
            })
    }

    /// §4d's footer: `Updated Ns ago` and `this machine only`.
    ///
    /// The age is measured against [`Self::process_stats_sampled_at`], the real instant the
    /// background poll last wrote a sample - never a render-time `Instant::now()`, which would
    /// always read "just now" and tell you nothing.
    fn render_resources_footer(&self) -> impl IntoElement {
        let since = self
            .process_stats_sampled_at
            .map(|at| std::time::Instant::now().saturating_duration_since(at));
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
                    .child("this machine only"),
            )
    }
}

/// The three tiers themselves - §4c's whole point is that they are genuinely three different
/// tones, in a genuine order. A "hierarchy" whose steps resolved to the same colour, or whose
/// primary was dimmer than its recessive, would be exactly the flat smear this rebuild removed.
#[cfg(test)]
mod status_bar_tier_tests {
    use super::*;

    /// Perceived lightness, good enough to order three greys of the same family.
    fn luminance(color: gpui::Rgba) -> f32 {
        0.2126 * color.r + 0.7152 * color.g + 0.0722 * color.b
    }

    #[test]
    fn the_three_tiers_are_three_distinct_tones_in_descending_order() {
        let primary = luminance(StatusTier::Primary.color().resolve());
        let secondary = luminance(StatusTier::Secondary.color().resolve());
        let recessive = luminance(StatusTier::Recessive.color().resolve());
        assert!(
            primary > secondary,
            "the primary tier must be brighter than the secondary one - {primary} vs {secondary}"
        );
        assert!(
            secondary > recessive,
            "the secondary tier must be brighter than the recessive one - {secondary} vs \
             {recessive}"
        );
    }

    /// A hierarchy is type *and* colour: the recessive tier is also the smaller one, so the tail
    /// of a split readout recedes even in a screenshot with no colour.
    #[test]
    fn the_recessive_tier_is_also_the_smaller_type() {
        assert!(StatusTier::Primary.text_size() > StatusTier::Recessive.text_size());
        assert_eq!(StatusTier::Primary.weight(), gpui::FontWeight::MEDIUM);
        assert_eq!(StatusTier::Recessive.weight(), gpui::FontWeight::NORMAL);
    }

    /// §4c: "every composite readout is split so it has a bright head and a dim tail". The branch
    /// cluster is the bar's one surviving composite, and the branch - not the arrow counts - is
    /// the head.
    #[test]
    fn the_branch_cluster_splits_into_a_branch_head_and_an_arrow_tail() {
        let split = branch_readout("main".to_string(), Some((2, 0)));
        assert_eq!(split.head, "main");
        assert_eq!(split.tail.as_deref(), Some("\u{2191}2 \u{2193}0"));
    }

    /// A not-yet-computed ahead/behind is honestly absent, never a fabricated `↑0 ↓0` - which
    /// would be indistinguishable from a real, measured "level with the base".
    #[test]
    fn an_unmeasured_ahead_behind_has_no_tail_at_all() {
        assert_eq!(branch_readout("main".to_string(), None).tail, None);
    }

    /// Rev 6 §7 rule 9, on the bar's one remaining count.
    #[test]
    fn the_running_agent_count_conjugates() {
        assert_eq!(running_agents_label(0), "0 agents running");
        assert_eq!(running_agents_label(1), "1 agent running");
        assert_eq!(running_agents_label(2), "2 agents running");
    }
}

/// The deletions §4b/§7 rule 5 demand, proved against the real, rendered bar: every one of these
/// readouts genuinely painted before this change, so a test that finds them gone is a test that
/// would have failed on the old bar.
#[cfg(test)]
mod status_bar_deletion_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    /// The editor-zoom readout is gone from the bar - and its *behaviour* is not. §4b's table:
    /// "editor zoom ... keyboard only (`⌘+`/`⌘−`); state and handlers kept, both controls gone".
    #[gpui::test]
    fn the_zoom_readout_is_gone_but_the_keyboard_zoom_still_works(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("status-bar-zoom-value").is_none(),
            "the status bar's editor-zoom readout must be gone - §4b deleted the control, not \
             just its label"
        );

        let before = app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent);
        app.update(cx, |app, cx| {
            app.zoom_in(cx);
            app.zoom_in(cx);
        });
        cx.run_until_parked();
        let zoomed = app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent);
        assert!(
            zoomed > before,
            "the zoom *state and handlers* must survive the readout's deletion - {before} -> \
             {zoomed}"
        );
        app.update(cx, |app, cx| app.reset_zoom(cx));
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            AdeApp::ZOOM_DEFAULT_PERCENT,
            "and so must the reset the deleted readout used to be the only button for"
        );
    }

    /// The three readouts §4b/§4d name, and the one the design keeps. Driven through the real
    /// rendered bar rather than by reading source: the point is that these elements are not on
    /// screen, whatever the code looks like.
    #[gpui::test]
    fn the_bar_keeps_exactly_the_segments_rev_six_kept(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        for kept in [
            "status-bar-branch-cluster",
            "status-bar-running-agents",
            "status-bar-resources",
        ] {
            assert!(
                cx.debug_bounds(kept).is_some(),
                "{kept} is on rev 6's kept list and must still paint"
            );
        }

        // And the whole file/editor cluster is gone with it - `status_bar_active_parsed_file`,
        // the shared gate that fed `ln N`/indent/EOL/encoding and the diagnostics count, no
        // longer exists at all, so opening a real file cannot bring any of them back.
        let file_path = repo.path().join("a.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write a.rs");
        app.update(cx, |app, _cx| {
            app.code_view = crate::code_surface::code_view::CodeView::File;
            app.open_change = Some(std::path::PathBuf::from("a.rs"));
            app.file_view_cache = crate::code_surface::code_view::load_file(&file_path).ok();
            app.file_view_error_count = Some(3);
            app.code_cursor = Some(7);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("status-bar-zoom-value").is_none(),
            "a real, open file must not resurrect any part of the deleted editor cluster"
        );
    }
}

/// Regression coverage for the status bar's branch cluster disappearing - and taking the git
/// graph's primary entry point with it - whenever no agent pane happened to be open.
///
/// `render_status_branch_cluster` opened with `self.agents.active()?`, which was never a
/// visibility decision: at Revision R6 the app had no `repos`/`focused_repo` at all, so the
/// active pane's `cwd` was simply the only way to look a branch up, and hiding the row when
/// there was no pane was an accident of that. The git graph tab later hung its click target and
/// fork glyph off the same function, promoting the leftover `?` into the gate for its own
/// button - while `AdeApp::open_git_graph` itself refuses only when `focused_repo()` is `None`.
/// So with a repo focused and every tab closed, the action worked and its only button was gone.
#[cfg(test)]
mod status_bar_branch_cluster_visibility_tests {
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
    }

    /// A real repo on a real branch, so the cluster has genuine branch text to show rather than
    /// falling through to `"(detached)"` for want of a git repository at all. Shared with
    /// `super::resources_popover_tests`, which needs the same real worktree list.
    pub(super) fn seeded_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1").expect("write a.txt");
        git(repo.path(), &["add", "a.txt"]);
        git(repo.path(), &["commit", "-m", "base"]);
        repo
    }

    #[gpui::test]
    fn the_branch_cluster_and_its_graph_button_survive_closing_every_agent(
        cx: &mut TestAppContext,
    ) {
        let repo = seeded_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // Sanity check on the starting state this regression is measured against: opening a repo
        // spawns a real startup shell, so there genuinely *is* an active agent here, and the
        // cluster paints. Without this the test could pass by never rendering at all.
        assert!(
            app.read_with(cx, |app, _| app.agents.active().is_some()),
            "sanity check: opening a repo spawns a real startup shell agent"
        );
        assert!(
            cx.debug_bounds("status-bar-branch-cluster").is_some(),
            "sanity check: the branch cluster paints while an agent is active"
        );

        // Close every agent through the real close path - the same one the tab's own ✕ uses -
        // rather than reaching into `Agents` and clearing it, so this reproduces a state the user
        // can genuinely reach.
        let ids: Vec<_> = app.read_with(cx, |app, _| {
            app.agents.iter().map(|agent| agent.id).collect()
        });
        app.update_in(cx, |app, window, cx| {
            for id in ids {
                app.close_agent(id, window, cx);
            }
        });
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.agents.active().is_none()),
            "sanity check: closing every tab genuinely leaves no active agent"
        );
        assert!(
            app.read_with(cx, |app, _| app.focused_repo().is_some()),
            "sanity check: the repo is still focused - this is the exact state where \
             open_git_graph still works but its button used to vanish"
        );

        // The regression itself.
        let bounds = cx.debug_bounds("status-bar-branch-cluster").expect(
            "the branch cluster - and so the git graph's status-bar button - must still render \
             with a repo focused and no agent open, because AdeApp::open_git_graph still works \
             in exactly this state",
        );

        // ...and it must be a genuinely live click target, not merely painted pixels.
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.graph_tab_open),
            "clicking the branch cluster with no agent open must really open the git graph tab, \
             through the same AdeApp::open_git_graph the + menu and palette use"
        );
    }
}

/// The Resources popover, driven through the real bar: a real click on the real readout, a real
/// tree built from the app's own real agents.
#[cfg(test)]
mod resources_popover_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    #[gpui::test]
    fn clicking_the_resources_readout_really_opens_and_closes_the_popover(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.resources_popover_open),
            "sanity check: the popover starts closed"
        );
        assert!(
            cx.debug_bounds("status-bar-resources-popover").is_none(),
            "sanity check: nothing is painted while it is closed"
        );

        let bounds = cx
            .debug_bounds("status-bar-resources")
            .expect("the resources readout must paint");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.resources_popover_open),
            "a real click on the readout must open the Resources popover"
        );
        assert!(
            cx.debug_bounds("status-bar-resources-popover").is_some(),
            "and the panel must really paint"
        );

        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.resources_popover_open),
            "a second click must close it again"
        );
    }

    /// §4d's `repo → worktree → agent`, built from the app's own real state: opening a real git
    /// repo spawns a real startup agent in a real worktree, so the tree must carry that agent
    /// under that repo's name, through the rail's own grouping, plus Jerry's own row.
    #[gpui::test]
    fn the_tree_is_keyed_repo_then_worktree_then_agent_and_includes_jerry_itself(
        cx: &mut TestAppContext,
    ) {
        let repo = super::status_bar_branch_cluster_visibility_tests::seeded_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let (tree, repo_name) = app.update(cx, |app, cx| {
            let name = app
                .repos
                .first()
                .map(|repo| repo.name.clone())
                .expect("a real focused repo");
            (app.build_resource_tree(cx), name)
        });

        assert!(
            tree.groups
                .iter()
                .any(|group| group.repo_name == JERRY_GROUP_LABEL),
            "Jerry's own process must be a real row - the readout promises what Jerry costs this \
             machine, and a total that excluded the window would not be that number"
        );
        let repo_group = tree
            .groups
            .iter()
            .find(|group| group.repo_name == repo_name)
            .expect("the opened repo must be a group of its own");
        assert!(
            !repo_group.rows.is_empty(),
            "the repo's real startup agent must appear under it"
        );
        // The middle level really is the *rail's* worktree, not a path basename guessed by the
        // fallback: this repo's one worktree is on branch `main`, and only the hierarchy walk
        // knows that.
        assert!(
            repo_group
                .rows
                .iter()
                .any(|row| row.worktree_label == "main"),
            "the agent must be attributed through the rail's own repo -> worktree hierarchy, \
             which is where the branch name comes from - got {:?}",
            repo_group
                .rows
                .iter()
                .map(|row| row.worktree_label.clone())
                .collect::<Vec<_>>()
        );
    }

    /// Every open agent with a real pid appears exactly once in the tree, whatever the rail's
    /// grouping currently knows about its worktree - a directory that is not a git repo at all
    /// still runs a real startup shell that really costs this machine something, and a readout
    /// claiming to be the whole cost must not silently drop it.
    #[gpui::test]
    fn every_open_agent_is_accounted_for_exactly_once(cx: &mut TestAppContext) {
        // Deliberately *not* a git repo: this is the case where the rail has no worktree rows to
        // hang the agent off, which is exactly when the fallback attribution has to carry it.
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let (tree, live_pids) = app.update(cx, |app, cx| {
            let pids: Vec<u32> = app
                .agents
                .iter()
                .filter_map(|agent| agent.pane.read(cx).pid())
                .collect();
            (app.build_resource_tree(cx), pids)
        });
        assert!(
            !live_pids.is_empty(),
            "sanity check: opening a repo really spawns a process with a real pid"
        );

        let tree_pids: Vec<u32> = tree.rows().map(|row| row.pid).collect();
        for pid in &live_pids {
            assert_eq!(
                tree_pids.iter().filter(|listed| *listed == pid).count(),
                1,
                "pid {pid} is a real, live agent process and must appear exactly once in the \
                 tree - dropping it makes the bar under-report, counting it twice makes it \
                 over-report"
            );
        }
        assert!(
            tree_pids.contains(&std::process::id()),
            "and Jerry's own process is in there too"
        );
    }

    /// The popover trigger carries **no** worktree or agent count (§3: "Do not put worktree or
    /// agent counts here; the rail footer already carries them 30px away"), and no meter (§4d).
    /// Checked against the real derived readout text.
    #[gpui::test]
    fn the_readout_carries_only_cost_no_counts(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let readout = app.update(cx, |app, cx| app.build_resource_tree(cx).bar_readout());
        assert!(
            readout.contains("cpu"),
            "the readout is the cost readout: {readout}"
        );
        for forbidden in ["worktree", "agent", "wt "] {
            assert!(
                !readout.contains(forbidden),
                "the resources readout must not carry a {forbidden} count - the rail footer \
                 already does, 30px away. Got: {readout}"
            );
        }
    }
}
