//! Pure logic for Zone 2 (work surface): tab strip, agent context bar, and the
//! CLI/terminal pane header/footer.
//!
//! Deliberately GPUI-free, mirroring `crate::rail::status`'s own split: this module only maps
//! already-known facts (a [`ProcessKind`], a [`Status`], a `bool`) onto *which*
//! colours/labels/actions a Zone 2 element should show, so that mapping is directly
//! unit-testable without a live GPUI window. Turning these into actual `gpui::Div` trees (and
//! wiring click handlers) happens one layer up, in `crate::root`, which has the
//! `Context<AdeApp>` these decisions need to act on.

use std::path::PathBuf;

use gpui::{Pixels, Rgba};

use crate::rail::status::Status;
use crate::sidebar::file_tree::LangChip;
use crate::theme;
use crate::work_surface::agents::{AgentId, AgentKind, ProcessKind};

/// One entry in a worktree's combined tab-strip order (GitHub issue #16, extended by issue #93
/// to also cover the git graph tab) - an agent tab, a file tab, or the one real git graph tab.
/// `Agents`/`crate::root::AdeApp::open_files`/`crate::root::AdeApp::graph_tab_open` remain the
/// real storage for *existence* and all process/buffer/graph-load behaviour - this only records
/// where a tab sits *visually*, so every kind can interleave in one strip instead of always
/// rendering as rigid, un-reorderable blocks. `Graph` carries no payload: unlike agents/files,
/// there is only ever at most one real graph tab per window (`crate::root::AdeApp::
/// graph_tab_open`'s own "one per window" docs), so there is nothing to distinguish one from
/// another the way an `AgentId`/`PathBuf` does for its own kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TabRef {
    Agent(AgentId),
    File(PathBuf),
    Graph,
    /// The agent review tab (GitHub issue #225), for the agent it reviews. Unlike `Graph` this
    /// carries a payload, because a review is inherently *per agent* - that is the entire point
    /// of the feature - even though, like `Graph`, at most one is open per window at a time
    /// (`crate::root::AdeApp::review_tab_open`).
    Review(AgentId),
    /// The run-transcript tab (GitHub issue #227), showing one finished run's own recording.
    Run,
}

/// Reconciles a worktree's stored tab order (`crate::root::AdeApp::tab_order`) against what's
/// *actually* open right now: drops any entry that no longer exists (a closed agent, a closed
/// file tab, a closed graph tab), then appends anything open that isn't in `stored` yet (a
/// freshly spawned agent, a freshly opened file, or - GitHub issue #93 - a freshly opened graph
/// tab) in `agents_for_cwd`/`open_files`/`graph_open`'s own order - the same "just append at the
/// end" position a brand new tab has always landed at. `graph_open` is a real, live "is the one
/// graph tab open right now" fact (`crate::root::AdeApp::graph_tab_open`), not itself scoped to
/// this worktree - only its *position within this worktree's own strip* is, matching how
/// `crate::root::AdeApp::tab_order` already remembers a different position per worktree for the
/// same shared agent/file tab identities never do (an agent/file *is* worktree-scoped; the graph
/// tab's own on-screen slot is what's being remembered here, not a second graph tab).
pub fn reconcile_tab_order(
    stored: &[TabRef],
    agents_for_cwd: &[AgentId],
    open_files: &[PathBuf],
    graph_open: bool,
    review_open: Option<AgentId>,
    run_open: bool,
) -> Vec<TabRef> {
    // GitHub issue #225: a review tab is live only when it's really open *and* the agent it
    // reviews is one of this worktree's own agents. Both halves matter: the first drops a closed
    // review tab exactly like every other kind, and the second keeps another worktree's review
    // tab out of this worktree's strip - `review_open` is a single window-wide slot (like
    // `graph_open`), but unlike the graph tab a review genuinely belongs to one worktree.
    let review_live = |id: &AgentId| review_open == Some(*id) && agents_for_cwd.contains(id);
    let mut order: Vec<TabRef> = stored
        .iter()
        .filter(|tab_ref| match tab_ref {
            TabRef::Agent(id) => agents_for_cwd.contains(id),
            TabRef::File(path) => open_files.contains(path),
            TabRef::Graph => graph_open,
            TabRef::Review(id) => review_live(id),
            // GitHub issue #227: `run_open` is already this worktree's own fact - the caller
            // looks it up in `run_tab_by_worktree` by cwd - so unlike `review_open` there is no
            // second "does it belong here" half to check.
            TabRef::Run => run_open,
        })
        .cloned()
        .collect();
    for id in agents_for_cwd {
        if !order
            .iter()
            .any(|tab_ref| matches!(tab_ref, TabRef::Agent(existing) if existing == id))
        {
            order.push(TabRef::Agent(*id));
        }
    }
    for path in open_files {
        if !order
            .iter()
            .any(|tab_ref| matches!(tab_ref, TabRef::File(existing) if existing == path))
        {
            order.push(TabRef::File(path.clone()));
        }
    }
    if graph_open && !order.contains(&TabRef::Graph) {
        order.push(TabRef::Graph);
    }
    if let Some(id) = review_open {
        if review_live(&id) && !order.contains(&TabRef::Review(id)) {
            order.push(TabRef::Review(id));
        }
    }
    if run_open && !order.contains(&TabRef::Run) {
        order.push(TabRef::Run);
    }
    order
}

/// Moves `dragged` to sit immediately before `target` (or immediately after it, if
/// `insert_after`) in `order` - the real backing for the unified tab strip's drag-to-reorder
/// gesture (`crate::root::AdeApp::reorder_tab`), used regardless of whether `dragged`/`target`
/// are the same kind (`TabRef::Agent`/`TabRef::File`) or not, which is GitHub issue #16's real
/// "any tab can cross into either group" ask - there is no separate per-kind reorder function to
/// keep in sync. A no-op if either entry is missing from `order`, or if they're the same entry.
pub fn move_tab_order(
    order: &mut Vec<TabRef>,
    dragged: &TabRef,
    target: &TabRef,
    insert_after: bool,
) {
    if dragged == target {
        return;
    }
    let Some(from) = order.iter().position(|tab_ref| tab_ref == dragged) else {
        return;
    };
    if !order.iter().any(|tab_ref| tab_ref == target) {
        return;
    }
    let item = order.remove(from);
    let mut to = order
        .iter()
        .position(|tab_ref| tab_ref == target)
        .unwrap_or(order.len());
    if insert_after {
        to += 1;
    }
    order.insert(to, item);
}

/// The real starting pixel offset for every tab whose horizontal slot moves as a side effect of
/// dragging `dragged` to land beside `target` - GitHub issue #16's own remaining gap (tracked
/// internally as task #65): before this, every tab other than the one actually dropped just
/// teleported to its new slot on the next render, with no visual feedback at all that a reorder
/// had happened to *them*. `crate::root::AdeApp::render_tab_chrome` interpolates each returned
/// tab's own `.left()` from this offset down to `0` (its already-correct new position - flexbox
/// itself, not this offset, is what actually re-seats a tab; this only makes the *seating* look
/// animated rather than instant) over a short, fixed duration, the same idiom
/// `crate::root::AdeApp::dropped_tab_settle`'s own settle-fade already uses for the dropped tab
/// itself.
pub fn tab_slide_offsets(
    old_order: &[TabRef],
    dragged: &TabRef,
    target: &TabRef,
    insert_after: bool,
    dragged_width: Pixels,
) -> Vec<(TabRef, Pixels)> {
    let mut new_order = old_order.to_vec();
    move_tab_order(&mut new_order, dragged, target, insert_after);

    let Some(old_index) = old_order.iter().position(|tab_ref| tab_ref == dragged) else {
        return Vec::new();
    };
    let Some(new_index) = new_order.iter().position(|tab_ref| tab_ref == dragged) else {
        return Vec::new();
    };
    if old_index == new_index {
        return Vec::new();
    }

    let (span_start, span_end) = if old_index < new_index {
        (old_index, new_index)
    } else {
        (new_index, old_index)
    };
    // Dragging rightward (`old_index < new_index`) removes `dragged` from in front of everything
    // between the two slots, so each of them visually starts `dragged_width` to the right of
    // (i.e. a *positive* offset from) its own already-correct new position and slides left to
    // `0`. Dragging leftward does the reverse: `dragged` lands in front of them, so each one
    // starts `dragged_width` to the left of its new position and slides right to `0`.
    let offset = if old_index < new_index {
        dragged_width
    } else {
        -dragged_width
    };

    old_order[span_start..=span_end]
        .iter()
        .filter(|tab_ref| *tab_ref != dragged)
        .map(|tab_ref| (tab_ref.clone(), offset))
        .collect()
}

/// Fully transparent - used for the "outline"/"ghost" button variants and an inactive tab's
/// background, so every button/tab can always call `.bg()`/`.border_color()` uniformly rather
/// than conditionally skipping the call (which would also shift the box model by the border's
/// width).
pub const TRANSPARENT: Rgba = Rgba {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.0,
};

/// The agent tint `(fg, bg)` for an agent's badge/chip - the one place an agent is turned into a
/// colour, so the same agent is the same colour on its rail badge, its CLI tab chip, its Changes
/// panel run rows and the conflict side headers.
pub fn agent_tint(kind: ProcessKind) -> (Rgba, Rgba) {
    match kind {
        // copper - `sonnet-4.5` in the mock's own `sessions`.
        ProcessKind::Agent(AgentKind::Claude) => {
            (theme::agent::SONNET.0.into(), theme::agent::SONNET.1.into())
        }
        // teal - `gpt-5-codex`.
        ProcessKind::Agent(AgentKind::Codex) => {
            (theme::agent::CODEX.0.into(), theme::agent::CODEX.1.into())
        }
        ProcessKind::Shell => (theme::text::DIM.into(), theme::surface::CHIP_NEUTRAL.into()),
    }
}

/// The agent badge's single-character initial.
pub fn agent_initial(kind: ProcessKind) -> &'static str {
    match kind {
        ProcessKind::Agent(AgentKind::Claude) => "C",
        ProcessKind::Agent(AgentKind::Codex) => "X",
        ProcessKind::Shell => "$",
    }
}

/// `kind`'s icon-pack file name (GitHub issue #5) - `crate::icon_pack::resolve_icon`'s own
/// `<name>.svg` lookup key. Deliberately its own, separate mapping from [`agent_initial`]'s
/// single-letter glyph rather than reusing that string directly: a pack author names files by
/// what the icon *depicts* (`"claude.svg"`), not by this app's own internal one-letter fallback
/// convention.
pub fn agent_icon_name(kind: ProcessKind) -> &'static str {
    match kind {
        ProcessKind::Agent(AgentKind::Claude) => "claude",
        ProcessKind::Agent(AgentKind::Codex) => "codex",
        ProcessKind::Shell => "shell",
    }
}

/// Which of the tab strip's two chip shapes an agent's tab draws: agent CLI gets a `❯` glyph,
/// terminal gets the pane glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabChipKind {
    Cli,
    Term,
}

pub fn tab_chip_kind(kind: ProcessKind) -> TabChipKind {
    match kind {
        ProcessKind::Agent(_) => TabChipKind::Cli,
        ProcessKind::Shell => TabChipKind::Term,
    }
}

/// A tab chip's `(bg, fg)`, active or dimmed. Dimmed reuses [`theme::border::ZONE`] (the same
/// token an inactive tab's own underline uses) for `bg`, and [`theme::text::FAINTER`] for `fg`.
#[derive(Debug, Clone, Copy)]
pub struct ChipColors {
    pub bg: Rgba,
    pub fg: Rgba,
}

pub fn tab_chip_colors(kind: ProcessKind, active: bool) -> ChipColors {
    if active {
        let (fg, bg) = agent_tint(kind);
        ChipColors { bg, fg }
    } else {
        ChipColors {
            bg: theme::border::ZONE.into(),
            fg: theme::text::FAINTER.into(),
        }
    }
}

/// A file tab's chip colours - the file's language chip when active, dimmed to the exact same
/// `bg`/`fg` [`tab_chip_colors`] dims an agent tab's chip to when inactive.
pub fn file_tab_chip_colors(lang: LangChip, active: bool) -> ChipColors {
    if active {
        ChipColors {
            bg: lang.bg,
            fg: lang.fg,
        }
    } else {
        ChipColors {
            bg: theme::border::ZONE.into(),
            fg: theme::text::FAINTER.into(),
        }
    }
}

/// A tab's own background/underline/label colour, active or inactive. The design's inactive
/// label colour (`#767d84`) has no exact token in `theme.rs`; [`theme::text::DIMMER`]
/// (`#7d848b`) is the closest ported token, used here rather than adding a new one-off constant.
#[derive(Debug, Clone, Copy)]
pub struct TabColors {
    pub bg: Rgba,
    pub underline: Rgba,
    pub label: Rgba,
}

/// An inactive tab's `underline` is the **window's column rule**, not a tab-level decoration -
/// which is why it is [`theme::border::RAIL_INNER`] and not [`theme::border::ZONE`]. GitHub issue
/// #291 / `design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4v: once the three column
/// headers line up, "**All three column headers are now 36** ... and all three borders are
/// `#191c1f` - the centre strip had been `#1e2225`, which once the rules lined up would have read
/// as one rule changing shade mid-span."
pub fn tab_colors(active: bool) -> TabColors {
    if active {
        TabColors {
            bg: theme::surface::CENTER.into(),
            underline: theme::surface::CENTER.into(),
            label: theme::text::PRIMARY.into(),
        }
    } else {
        TabColors {
            bg: TRANSPARENT,
            underline: theme::border::RAIL_INNER.into(),
            label: theme::text::DIMMER.into(),
        }
    }
}

/// The CLI/terminal pane header's pty-state text (`attached · waiting on stdin` / `attached ·
/// streaming` / `exited N` / `not started`). This app has no detach/resume concept (an agent is
/// exactly one live process for its whole lifetime - see `crate::work_surface::agents`), so a `detached ·
/// resumable` state is never produced. Reads the same `is_running`/exit-code facts
/// `crate::rail::status::derive_status` consumes, rather than a second heuristic that could drift from
/// the status pill shown right next to it.
pub fn pty_state_label(is_running: bool, status: Status, exit_code: Option<u32>) -> String {
    if !is_running {
        return match exit_code {
            Some(code) => format!("exited {code}"),
            None => "not started".to_string(),
        };
    }
    match status {
        Status::Ask => "attached \u{b7} waiting on stdin".to_string(),
        Status::Idle => "attached \u{b7} idle".to_string(),
        // Fail/Review only arise from `ProcessSignal::Exited`, which implies `is_running ==
        // false` - unreachable here in practice, but matched explicitly so a future status
        // variant doesn't silently fall through a wildcard arm.
        Status::Run | Status::Fail | Status::Review => "attached \u{b7} streaming".to_string(),
    }
}

/// A tab's label: what the process inside that pane says it is *right now*.
pub fn live_tab_label(title: Option<&str>, program: &str) -> String {
    match title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => title.to_string(),
        None => program.to_string(),
    }
}

/// The `+` menu's "New agent" row secondary text (`design_handoff_jerry_ade/revision 3/
/// REVISION-2026-07-31.md` §3: "*New agent* (`runs in <branch>`)") - the real, currently selected
/// worktree's branch substituted in, never a hardcoded model/kind name (that was the pre-fix
/// bug: the row showed `agent.kind.label()`, e.g. `"Claude"`, which is not what this spec item
/// asks for at all). Falls back to `(detached)` for a worktree with no recorded branch, mirroring
/// `crate::work_surface::render::AdeApp::render_agent_context_bar`'s own branch fallback so the
/// two don't invent two different placeholder strings for the same "no branch" fact.
pub fn new_agent_menu_secondary_text(branch: Option<&str>) -> String {
    format!("runs in {}", branch.unwrap_or("(detached)"))
}

/// A footer action button's colour treatment, backed by `theme::button::*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStyle {
    PrimaryGreen,
    PrimaryBlue,
    Outline,
    Ghost,
}

#[derive(Debug, Clone, Copy)]
pub struct ActionColors {
    pub bg: Rgba,
    pub border: Rgba,
    pub fg: Rgba,
    pub keycap_fg: Rgba,
    pub keycap_border: Rgba,
}

pub fn action_button_colors(style: ActionStyle) -> ActionColors {
    match style {
        ActionStyle::PrimaryGreen => ActionColors {
            bg: theme::button::GREEN_BG.into(),
            border: theme::button::GREEN_BG.into(),
            fg: theme::button::GREEN_FG.into(),
            keycap_fg: theme::button::GREEN_KEYCAP_FG.into(),
            keycap_border: theme::button::GREEN_KEYCAP.into(),
        },
        ActionStyle::PrimaryBlue => ActionColors {
            bg: theme::button::BLUE_BG.into(),
            border: theme::button::BLUE_BG.into(),
            fg: theme::button::BLUE_FG.into(),
            // Same blue (`#8fbde6`) as `term::PROMPT`, reused rather than duplicated.
            keycap_fg: theme::term::PROMPT.into(),
            keycap_border: theme::button::BLUE_KEYCAP.into(),
        },
        ActionStyle::Outline => ActionColors {
            bg: TRANSPARENT,
            border: theme::border::BUTTON.into(),
            fg: theme::text::SECONDARY.into(),
            keycap_fg: theme::text::DIMMER.into(),
            keycap_border: theme::border::BUTTON.into(),
        },
        ActionStyle::Ghost => ActionColors {
            bg: TRANSPARENT,
            border: TRANSPARENT,
            fg: theme::text::DIMMER.into(),
            keycap_fg: theme::text::FAINT.into(),
            keycap_border: theme::border::KEYCAP.into(),
        },
    }
}

/// Which operation a footer action button performs, if any - `crate::root::AdeApp`'s click
/// handlers dispatch on this. Every variant either has real backing logic wired up, or is
/// rendered honestly disabled (see [`FooterAction::implemented`]) - never a button that looks
/// clickable but silently does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Closes this tab and spawns a fresh agent of the same kind/cwd - an approximate
    /// stand-in for `Retry`/`Resume` (this app has no saved-agent resumability to actually
    /// resume *from* - see [`pty_state_label`] on the same gap).
    Respawn,
    /// `crate::worktree_history::flow::AdeApp::request_discard_worktree` (Revision R10): a real
    /// `wt_core::undo::discard_worktree`, behind the same two-click confirmation as the rail
    /// footer's `prune` button (see that method's own docs for why - this is a real, destructive
    /// action that force-removes a worktree, preserving uncommitted/untracked content in a real
    /// git stash first).
    DiscardWorktree,
}

#[derive(Debug, Clone, Copy)]
pub struct FooterAction {
    pub kind: ActionKind,
    pub label: &'static str,
    /// A keybinding **spec string** (`"mod+enter"`, `"ctrl+C"`), not an already-resolved glyph -
    /// the render call site runs it through `crate::keymap::resolve_combo`, so the same spec
    /// renders `⌘⏎`/`⌃C` on macOS and `Ctrl Enter`/`Ctrl C` on Windows/Linux.
    pub keycap: Option<&'static str>,
    pub style: ActionStyle,
    /// Whether this action kind has real backing logic wired up at all (a *static* fact,
    /// independent of this agent's current state - the render call site layers further,
    /// state-dependent enablement on top of this). `false` always means rendered dimmed and
    /// non-interactive, never a clickable-looking no-op.
    pub implemented: bool,
}

/// The footer action strip for one [`Status`] - GitHub issue #295's final state
/// (`STAGE-A-CHANGELOG.md` §4e/§4r/§4t).
pub fn footer_actions(status: Status) -> Vec<FooterAction> {
    match status {
        // §4r: "`review` now renders no bar, matching `ask`". `Keep all` was "a fiction borrowed
        // from buffer-based inline-diff tools" (the edits are already on disk), `Review`/`Open in
        // editor` were navigation, and `Discard worktree` under one agent of a two-agent worktree
        // "throws away the other agent's work from a pane that never mentions them".
        Status::Review => Vec::new(),
        // §4e: "`Interrupt` offered on an agent that is *waiting for you* - there is nothing to
        // interrupt", and a second terminal does not answer the question the agent is asking.
        Status::Ask => Vec::new(),
        Status::Fail => vec![
            FooterAction {
                kind: ActionKind::Respawn,
                label: "Retry",
                keycap: Some("mod+R"),
                style: ActionStyle::Outline,
                implemented: true,
            },
            FooterAction {
                kind: ActionKind::DiscardWorktree,
                label: "Discard worktree",
                keycap: None,
                style: ActionStyle::Ghost,
                implemented: true,
            },
        ],
        // §4t: "`Interrupt` was the last button on it, and the pane is a terminal: `⌃C` already
        // interrupts and `mod+R` already retries, so a button duplicating a keystroke that works
        // in the focused surface is the same unearned space as §4r."
        Status::Run => Vec::new(),
        Status::Idle => vec![FooterAction {
            kind: ActionKind::Respawn,
            label: "Resume",
            keycap: Some("mod+enter"),
            style: ActionStyle::PrimaryBlue,
            implemented: true,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    #[test]
    fn agent_kinds_get_the_cli_chip_and_shell_gets_the_terminal_chip() {
        assert_eq!(tab_chip_kind(ProcessKind::claude()), TabChipKind::Cli);
        assert_eq!(tab_chip_kind(ProcessKind::codex()), TabChipKind::Cli);
        assert_eq!(tab_chip_kind(ProcessKind::Shell), TabChipKind::Term);
    }

    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }

    #[test]
    fn an_active_cli_chip_is_tinted_with_its_own_agent_colour_not_a_shared_default() {
        let claude = tab_chip_colors(ProcessKind::claude(), true);
        let codex = tab_chip_colors(ProcessKind::codex(), true);
        assert!(
            !same(claude.bg, codex.bg),
            "two different agents must not share a tab chip colour"
        );
        let (claude_fg, claude_bg) = theme::agent::SONNET;
        assert!(same(claude.fg, claude_fg.into()));
        assert!(same(claude.bg, claude_bg.into()));
    }

    #[test]
    fn an_active_file_tab_chip_is_tinted_with_its_own_language_colour() {
        let rs = LangChip {
            label: "rs",
            fg: theme::lang::RS.0.into(),
            bg: theme::lang::RS.1.into(),
        };
        let colors = file_tab_chip_colors(rs, true);
        assert!(same(colors.fg, theme::lang::RS.0.into()));
        assert!(same(colors.bg, theme::lang::RS.1.into()));
    }

    #[test]
    fn an_inactive_file_tab_chip_is_dimmed_to_the_same_neutral_a_agent_tab_chip_uses() {
        let rs = LangChip {
            label: "rs",
            fg: theme::lang::RS.0.into(),
            bg: theme::lang::RS.1.into(),
        };
        let file_colors = file_tab_chip_colors(rs, false);
        let agent_colors = tab_chip_colors(ProcessKind::Shell, false);
        assert!(same(file_colors.bg, agent_colors.bg));
        assert!(same(file_colors.fg, agent_colors.fg));
    }

    #[test]
    fn an_inactive_chip_is_always_dimmed_to_the_same_neutral_regardless_of_kind() {
        let claude = tab_chip_colors(ProcessKind::claude(), false);
        let shell = tab_chip_colors(ProcessKind::Shell, false);
        assert!(same(claude.bg, shell.bg));
        assert!(same(claude.fg, shell.fg));
        assert!(same(claude.bg, theme::border::ZONE.into()));
    }

    #[test]
    fn active_tab_background_and_underline_are_the_same_colour_so_it_merges_into_the_surface() {
        let active = tab_colors(true);
        assert!(
            same(active.bg, active.underline),
            "an active tab's underline must match its own background - that's how it visually \
             merges into the surface below it, per design_handoff_jerry_ade/README.md"
        );
    }

    #[test]
    fn inactive_tab_background_is_transparent_not_the_active_colour() {
        let inactive = tab_colors(false);
        assert!(same(inactive.bg, TRANSPARENT));
        assert!(!same(inactive.underline, tab_colors(true).underline));
    }

    #[test]
    fn a_process_that_never_started_reports_not_started_not_a_false_exit() {
        assert_eq!(pty_state_label(false, Status::Idle, None), "not started");
    }

    #[test]
    fn an_exited_process_always_reports_its_real_exit_code() {
        assert_eq!(
            pty_state_label(false, Status::Fail, Some(101)),
            "exited 101"
        );
        assert_eq!(pty_state_label(false, Status::Review, Some(0)), "exited 0");
    }

    #[test]
    fn a_running_agent_past_the_ask_threshold_reports_waiting_on_stdin() {
        assert_eq!(
            pty_state_label(true, Status::Ask, None),
            "attached \u{b7} waiting on stdin"
        );
    }

    #[test]
    fn a_running_and_recently_active_agent_reports_streaming() {
        assert_eq!(
            pty_state_label(true, Status::Run, None),
            "attached \u{b7} streaming"
        );
    }

    #[test]
    fn a_running_idle_shell_reports_idle_not_streaming_or_exited() {
        assert_eq!(
            pty_state_label(true, Status::Idle, None),
            "attached \u{b7} idle"
        );
    }

    #[test]
    fn every_surviving_footer_action_has_real_backing_logic() {
        for status in Status::ORDER {
            for action in footer_actions(status) {
                assert!(
                    action.implemented,
                    "{status:?}'s {:?} action is not implemented - issue #295 deleted the \
                     placeholder row rather than keeping an inert button",
                    action.label
                );
            }
        }
    }

    #[test]
    fn a_finished_agent_offers_no_footer_actions_at_all() {
        assert!(footer_actions(Status::Review).is_empty());
    }

    #[test]
    fn an_asking_agent_offers_no_footer_actions_at_all() {
        assert!(footer_actions(Status::Ask).is_empty());
    }

    #[test]
    fn a_running_agent_offers_no_footer_actions_at_all() {
        assert!(footer_actions(Status::Run).is_empty());
    }

    #[test]
    fn fail_actions_are_exactly_a_real_retry_then_a_real_discard() {
        let actions = footer_actions(Status::Fail);
        let labels: Vec<&str> = actions.iter().map(|action| action.label).collect();
        assert_eq!(labels, vec!["Retry", "Discard worktree"]);
        assert_eq!(actions[0].kind, ActionKind::Respawn);
        assert_eq!(actions[1].kind, ActionKind::DiscardWorktree);
        assert!(actions.iter().all(|action| action.implemented));
    }

    #[test]
    fn idle_actions_are_just_a_real_resume() {
        let actions = footer_actions(Status::Idle);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, ActionKind::Respawn);
        assert_eq!(actions[0].label, "Resume");
        assert!(actions[0].implemented);
    }

    #[test]
    fn no_status_offers_a_verb_that_lives_somewhere_else_now() {
        for status in Status::ORDER {
            for action in footer_actions(status) {
                assert!(
                    !matches!(
                        action.label,
                        "Interrupt"
                            | "Open terminal"
                            | "Keep all"
                            | "Review"
                            | "Review diff"
                            | "Open in editor"
                            | "Merge"
                            | "Archive"
                    ),
                    "{status:?} still offers {:?}, which GitHub issue #295 moved out of the \
                     agent pane's bottom strip",
                    action.label
                );
            }
        }
    }

    #[test]
    fn surviving_buttons_advertise_only_keycaps_that_really_exist() {
        let fail = footer_actions(Status::Fail);
        assert_eq!(fail[0].keycap, Some("mod+R"));
        assert_eq!(fail[1].keycap, None);
        assert_eq!(footer_actions(Status::Idle)[0].keycap, Some("mod+enter"));
    }

    #[test]
    fn a_live_title_is_the_whole_tab_label_with_nothing_appended() {
        assert_eq!(
            live_tab_label(Some("\u{2733} Claude Code"), "claude"),
            "\u{2733} Claude Code"
        );
        assert_eq!(live_tab_label(Some("~/src/jerry"), "zsh"), "~/src/jerry");
    }

    #[test]
    fn a_pane_whose_process_has_set_no_title_falls_back_to_its_program_name() {
        assert_eq!(live_tab_label(None, "zsh"), "zsh");
    }

    #[test]
    fn an_empty_or_whitespace_title_falls_back_rather_than_rendering_a_blank_tab() {
        assert_eq!(live_tab_label(Some(""), "bash"), "bash");
        assert_eq!(live_tab_label(Some("   "), "bash"), "bash");
    }

    #[test]
    fn two_panes_reporting_the_same_title_get_the_same_label() {
        assert_eq!(
            live_tab_label(Some("nvim"), "zsh"),
            live_tab_label(Some("nvim"), "bash"),
        );
    }

    #[test]
    fn the_new_agent_menu_row_shows_the_real_branch_never_a_model_name() {
        assert_eq!(
            new_agent_menu_secondary_text(Some("feature/real-branch")),
            "runs in feature/real-branch"
        );
        assert_ne!(
            new_agent_menu_secondary_text(Some("feature/real-branch")),
            "Claude",
            "must never show a model/agent-kind label in place of the branch"
        );
    }

    #[test]
    fn the_new_agent_menu_row_falls_back_to_detached_with_no_recorded_branch() {
        assert_eq!(new_agent_menu_secondary_text(None), "runs in (detached)");
    }

    #[test]
    fn action_button_colours_are_distinct_per_style() {
        let styles = [
            ActionStyle::PrimaryGreen,
            ActionStyle::PrimaryBlue,
            ActionStyle::Outline,
            ActionStyle::Ghost,
        ];
        let colors: Vec<ActionColors> = styles.iter().map(|s| action_button_colors(*s)).collect();
        for (i, a) in colors.iter().enumerate() {
            for (j, b) in colors.iter().enumerate() {
                if i != j {
                    assert!(
                        !same(a.fg, b.fg) || !same(a.bg, b.bg),
                        "action styles {i} and {j} are visually indistinguishable"
                    );
                }
            }
        }
    }

    #[test]
    fn reconcile_with_no_stored_order_appends_agents_then_files_in_their_own_order() {
        let order = reconcile_tab_order(
            &[],
            &[1, 2],
            &[PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            false,
            None,
            false,
        );
        assert_eq!(
            order,
            vec![
                TabRef::Agent(1),
                TabRef::Agent(2),
                TabRef::File(PathBuf::from("a.rs")),
                TabRef::File(PathBuf::from("b.rs")),
            ]
        );
    }

    #[test]
    fn reconcile_preserves_a_stored_interleaved_order() {
        let stored = vec![
            TabRef::Agent(1),
            TabRef::File(PathBuf::from("a.rs")),
            TabRef::Agent(2),
        ];
        let order = reconcile_tab_order(
            &stored,
            &[1, 2],
            &[PathBuf::from("a.rs")],
            false,
            None,
            false,
        );
        assert_eq!(order, stored);
    }

    #[test]
    fn reconcile_drops_entries_that_no_longer_exist() {
        let stored = vec![
            TabRef::Agent(1),
            TabRef::File(PathBuf::from("a.rs")),
            TabRef::Agent(2),
        ];
        let order = reconcile_tab_order(&stored, &[2], &[], false, None, false);
        assert_eq!(order, vec![TabRef::Agent(2)]);
    }

    #[test]
    fn reconcile_appends_newly_opened_tabs_not_yet_in_the_stored_order() {
        let stored = vec![TabRef::Agent(1)];
        let order = reconcile_tab_order(
            &stored,
            &[1, 2],
            &[PathBuf::from("a.rs")],
            false,
            None,
            false,
        );
        assert_eq!(
            order,
            vec![
                TabRef::Agent(1),
                TabRef::Agent(2),
                TabRef::File(PathBuf::from("a.rs")),
            ]
        );
    }

    #[test]
    fn reconcile_appends_a_freshly_opened_graph_tab() {
        let order = reconcile_tab_order(&[], &[1], &[], true, None, false);
        assert_eq!(order, vec![TabRef::Agent(1), TabRef::Graph]);
    }

    #[test]
    fn reconcile_preserves_a_stored_graph_tab_position() {
        let stored = vec![TabRef::Graph, TabRef::Agent(1)];
        let order = reconcile_tab_order(&stored, &[1], &[], true, None, false);
        assert_eq!(order, stored);
    }

    #[test]
    fn reconcile_drops_a_closed_graph_tab() {
        let stored = vec![TabRef::Agent(1), TabRef::Graph];
        let order = reconcile_tab_order(&stored, &[1], &[], false, None, false);
        assert_eq!(order, vec![TabRef::Agent(1)]);
    }

    #[test]
    fn reconcile_appends_a_freshly_opened_review_tab() {
        let order = reconcile_tab_order(&[], &[1], &[], false, Some(1), false);
        assert_eq!(order, vec![TabRef::Agent(1), TabRef::Review(1)]);
    }

    #[test]
    fn reconcile_preserves_a_stored_review_tab_position() {
        let stored = vec![TabRef::Review(1), TabRef::Agent(1)];
        assert_eq!(
            reconcile_tab_order(&stored, &[1], &[], false, Some(1), false),
            stored
        );
    }

    #[test]
    fn reconcile_drops_a_closed_review_tab() {
        let stored = vec![TabRef::Agent(1), TabRef::Review(1)];
        let order = reconcile_tab_order(&stored, &[1], &[], false, None, false);
        assert_eq!(order, vec![TabRef::Agent(1)]);
    }

    #[test]
    fn reconcile_drops_a_review_tab_whose_agent_is_gone() {
        let stored = vec![TabRef::Review(7)];
        assert!(reconcile_tab_order(&stored, &[], &[], false, Some(7), false).is_empty());
    }

    #[test]
    fn a_review_tab_never_leaks_into_another_worktrees_strip() {
        let order = reconcile_tab_order(&[], &[2], &[], false, Some(1), false);
        assert_eq!(order, vec![TabRef::Agent(2)]);
    }

    #[test]
    fn reconcile_treats_the_run_tab_like_every_other_kind() {
        assert_eq!(
            reconcile_tab_order(&[], &[1], &[], false, None, true),
            vec![TabRef::Agent(1), TabRef::Run],
            "a freshly opened run tab lands last, like every other new tab"
        );

        let stored = vec![TabRef::Run, TabRef::Agent(1)];
        assert_eq!(
            reconcile_tab_order(&stored, &[1], &[], false, None, true),
            stored,
            "and a dragged position is honoured rather than re-appended"
        );

        assert_eq!(
            reconcile_tab_order(&stored, &[1], &[], false, None, false),
            vec![TabRef::Agent(1)],
            "a closed run tab is dropped, not left dangling in the strip"
        );
    }

    #[test]
    fn a_worktree_strip_can_never_hold_two_run_tabs() {
        let once = reconcile_tab_order(&[], &[1], &[], false, None, true);
        let twice = reconcile_tab_order(&once, &[1], &[], false, None, true);
        assert_eq!(
            once, twice,
            "reconciliation is idempotent for the run tab too"
        );
        assert_eq!(
            twice.iter().filter(|tab| **tab == TabRef::Run).count(),
            1,
            "\u{a7}3: one run tab per worktree, replaced on the next open"
        );
    }

    #[test]
    fn move_tab_order_drops_a_file_tab_before_a_agent_tab() {
        let mut order = vec![
            TabRef::Agent(1),
            TabRef::Agent(2),
            TabRef::File(PathBuf::from("a.rs")),
        ];
        move_tab_order(
            &mut order,
            &TabRef::File(PathBuf::from("a.rs")),
            &TabRef::Agent(2),
            false,
        );
        assert_eq!(
            order,
            vec![
                TabRef::Agent(1),
                TabRef::File(PathBuf::from("a.rs")),
                TabRef::Agent(2),
            ]
        );
    }

    #[test]
    fn move_tab_order_respects_insert_after() {
        let mut order = vec![TabRef::Agent(1), TabRef::Agent(2), TabRef::Agent(3)];
        move_tab_order(&mut order, &TabRef::Agent(1), &TabRef::Agent(2), true);
        assert_eq!(
            order,
            vec![TabRef::Agent(2), TabRef::Agent(1), TabRef::Agent(3)]
        );
    }

    #[test]
    fn move_tab_order_is_a_no_op_for_an_unknown_or_identical_entry() {
        let original = vec![TabRef::Agent(1), TabRef::File(PathBuf::from("a.rs"))];
        let mut order = original.clone();
        move_tab_order(&mut order, &TabRef::Agent(1), &TabRef::Agent(1), false);
        move_tab_order(&mut order, &TabRef::Agent(99), &TabRef::Agent(1), false);
        move_tab_order(&mut order, &TabRef::Agent(1), &TabRef::Agent(99), false);
        assert_eq!(order, original);
    }

    #[test]
    fn tab_slide_offsets_slides_every_passed_over_tab_left_by_the_dragged_tabs_own_width() {
        let order = vec![TabRef::Agent(1), TabRef::Agent(2), TabRef::Agent(3)];
        let width = px(40.0);

        let slides = tab_slide_offsets(&order, &TabRef::Agent(1), &TabRef::Agent(3), true, width);

        assert_eq!(
            slides,
            vec![(TabRef::Agent(2), width), (TabRef::Agent(3), width)]
        );
    }

    #[test]
    fn tab_slide_offsets_slides_every_passed_over_tab_right_when_dragging_leftward() {
        let order = vec![TabRef::Agent(1), TabRef::Agent(2), TabRef::Agent(3)];
        let width = px(40.0);

        let slides = tab_slide_offsets(&order, &TabRef::Agent(3), &TabRef::Agent(1), false, width);

        assert_eq!(
            slides,
            vec![(TabRef::Agent(1), -width), (TabRef::Agent(2), -width)]
        );
    }

    #[test]
    fn tab_slide_offsets_never_slides_a_tab_outside_the_passed_over_span() {
        let order = vec![
            TabRef::Agent(1),
            TabRef::Agent(2),
            TabRef::Agent(3),
            TabRef::Agent(4),
        ];
        let width = px(25.0);

        let slides = tab_slide_offsets(&order, &TabRef::Agent(1), &TabRef::Agent(2), true, width);

        assert_eq!(
            slides,
            vec![(TabRef::Agent(2), width)],
            "only tab 2, the one real tab dragged-tab 1 passed over, may slide - tabs 3 and 4 \
             never changed position"
        );
    }

    #[test]
    fn tab_slide_offsets_is_empty_when_the_drop_lands_on_the_tabs_own_already_adjacent_slot() {
        let order = vec![
            TabRef::Agent(1),
            TabRef::Agent(2),
            TabRef::Agent(3),
            TabRef::Agent(4),
        ];
        let width = px(25.0);

        let slides = tab_slide_offsets(&order, &TabRef::Agent(1), &TabRef::Agent(2), false, width);

        assert!(slides.is_empty());
    }

    #[test]
    fn tab_slide_offsets_is_empty_for_every_move_tab_order_no_op() {
        let order = vec![TabRef::Agent(1), TabRef::Agent(2)];
        let width = px(40.0);

        assert!(
            tab_slide_offsets(&order, &TabRef::Agent(1), &TabRef::Agent(1), false, width)
                .is_empty()
        );
        assert!(
            tab_slide_offsets(&order, &TabRef::Agent(99), &TabRef::Agent(1), false, width)
                .is_empty()
        );
        assert!(
            tab_slide_offsets(&order, &TabRef::Agent(1), &TabRef::Agent(99), false, width)
                .is_empty()
        );
    }
}
