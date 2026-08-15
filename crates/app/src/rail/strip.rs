//! The sidebar strip, as pure data (GitHub issue #291): which views the left column offers, which
//! cells the strip really paints, what each cell's marker and tooltip say, and the one gate that
//! empties the strip on a day with no worktrees.
//!
//! GPUI-free, like every other `state`-shaped module in this folder - "does an empty day still
//! claim three agents need a human" and "does the Problems cell mark red or amber" are decisions
//! worth asserting without a window. [`crate::rail::strip_render`] is the `impl AdeApp` half: the
//! real 36px band, the real switch, and the real Problems body.
//!
//! ## What the strip is
//!
//! `design_handoff_jerry_ade/revision 5/REVISION-2026-08-13.md` §1 introduced it - "The left panel
//! was hard-wired to one thing. It is now a switchable surface with a **horizontal icon strip
//! along the top** - Cursor's arrangement, not VS Code's vertical activity bar. A vertical bar
//! costs 44px of permanent width on a window whose whole job is fitting three panels side by
//! side" - and `STAGE-A-CHANGELOG.md` §4v is its final form, which this module implements.
//!
//! §1's draft had four view cells (Worktrees, History, Search, Problems) plus a Settings gear.
//! §4u/§4v supersede that and are what ships: **Search moved to the right panel** ("Search is now
//! the middle tab of the right panel"), **History moved into the `⋯` overflow** ("a permanent cell
//! in a 5-cell strip is a claim that you switch to it constantly. If you don't, it belongs in the
//! overflow"), and Settings went with it. What is left is [`SidebarView`]'s two entries - which is
//! why this enum has two variants and not four.
//!
//! ## The gate is here, not in the renderer
//!
//! §1, verbatim: "On **First run** and **Empty day** the icon strip drops its four buttons and
//! keeps only the `+` action; the sidebar shows the rail's own empty state. With no worktrees
//! there are no views to offer, and a switcher with four dead views is worse than no switcher."
//! And: "Gate this **at the source** - `sideItems` and each view's `show*` flag - not in the
//! template. The badges and the History/Search/Problems bodies derive from `sessions`, `histDefs`,
//! `searchHits` and the diagnostics list; ungated they claimed 3 agents needing a human and 4
//! problems on a day the rail, title bar and footer all reported zero."
//!
//! [`strip_view_cells`] is that source. It takes the real counts and returns an empty `Vec` when
//! there is nothing to switch between, so there is no `when(..)` in the renderer that could be
//! written once for the cells and forgotten for the badges.

use crate::icons::Icon;
use crate::root::plural;
use crate::theme;

/// A view the left column can show. Exactly the two §4u leaves in the strip.
///
/// [`SidebarView::Worktrees`] is the rail this app has always had, unchanged - §2's table says so
/// in as many words ("the existing repo → worktree → agent tree, unchanged"). The strip does not
/// rebuild it; it sits above it and gates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SidebarView {
    /// The repo → worktree → agent tree.
    #[default]
    Worktrees,
    /// The selected worktree's LSP diagnostics.
    Problems,
}

impl SidebarView {
    /// Every view, in the order the strip paints them.
    pub const ALL: &'static [SidebarView] = &[SidebarView::Worktrees, SidebarView::Problems];

    /// The view's name - the first half of its `"<view> — <hint>"` tooltip, and the word the
    /// Problems body's own copy uses.
    pub const fn label(self) -> &'static str {
        match self {
            SidebarView::Worktrees => "Worktrees",
            SidebarView::Problems => "Problems",
        }
    }

    /// The view's hint - the second half of its tooltip, quoted from `Jerry.dc.html`'s own
    /// `sideViews` table. §1 explains why every cell needs one: "With labels gone, [the] glyphs
    /// are the only affordance identifying the views, so the hint has to live somewhere
    /// reachable."
    pub const fn hint(self) -> &'static str {
        match self {
            SidebarView::Worktrees => "repo \u{b7} worktree \u{b7} agent",
            SidebarView::Problems => "diagnostics in this worktree",
        }
    }

    /// The view's glyph, from `REVISION-2026-08-14.md` §8's mapping table (GitHub issue #282):
    /// "strip: worktrees / history / problems → `tree-structure` / `clock-counter-clockwise` /
    /// `warning`". Drawn through [`crate::icons::IconRow`] at [`crate::icons::IconSize::Strip`],
    /// which is what §7 rule 7's "one shared optical box, not one size per icon" means in code.
    pub const fn icon(self) -> Icon {
        match self {
            SidebarView::Worktrees => Icon::TreeStructure,
            SidebarView::Problems => Icon::Warning,
        }
    }

    /// The cell's `title="<view> — <hint>"` tooltip (§1), with the marker's real count appended in
    /// parentheses when there is one - `Jerry.dc.html`'s own
    /// `v.label + ' — ' + v.hint + (badge ? ' (' + badge + ')' : '')`.
    ///
    /// The count living *here* rather than on the cell is §4v's own correction: "The 9px pill
    /// printing a count at 7px sat on top of the glyph and was a second vocabulary for state in a
    /// strip built to match the tabs - which already mark state with a **5px square dot**. That is
    /// what the cells use now, in the badge's colour, with the count moved to the tooltip."
    pub fn tooltip(self, marker: Option<StripMarker>) -> String {
        match marker {
            Some(marker) => format!(
                "{} \u{2014} {} ({})",
                self.label(),
                self.hint(),
                marker.count
            ),
            None => format!("{} \u{2014} {}", self.label(), self.hint()),
        }
    }
}

/// Which hue a cell's state marker takes. Both are the app's own existing status hues, not a
/// one-off pair - `REVISION-2026-08-14.md` §6: "badges use the app's own hues (amber `#e2a336` for
/// worktrees, red `#e0625c` for problems), not a one-off cream."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerTone {
    /// Amber: work that is waiting on a human.
    NeedsYou,
    /// Red: something has failed or errored.
    Failure,
}

impl MarkerTone {
    /// The real token this tone paints with.
    pub const fn color(self) -> theme::ColorToken {
        match self {
            MarkerTone::NeedsYou => theme::status::ASK,
            MarkerTone::Failure => theme::status::FAIL,
        }
    }
}

/// A cell's state marker: the tabs' own 5px square dot, in a status hue, with its real count.
///
/// Never constructed at zero - see [`StripMarker::new`]. §1's rule for the badge it replaced still
/// governs: "Hidden at zero."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripMarker {
    /// How many things the marker is standing for. Shown in the tooltip, never on the cell.
    pub count: usize,
    pub tone: MarkerTone,
}

impl StripMarker {
    /// A marker for `count` things, or `None` at zero.
    ///
    /// Total on purpose: every caller here would otherwise repeat `(count > 0).then(..)`, and the
    /// one that forgot would paint a dot standing for nothing.
    pub fn new(count: usize, tone: MarkerTone) -> Option<Self> {
        (count > 0).then_some(StripMarker { count, tone })
    }
}

/// One cell the strip really paints for a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripCell {
    pub view: SidebarView,
    /// Whether this is the view the sidebar is showing - the cell that fills with the rail's own
    /// background and cuts the column rule beneath it.
    pub selected: bool,
    pub marker: Option<StripMarker>,
}

impl StripCell {
    /// The colour this cell paints the window's column rule in - **the cut-out**.
    ///
    /// `STAGE-A-CHANGELOG.md` §4v: selected is "a filled slab that cuts the column rule to join
    /// the panel below", achieved by drawing that rule in the panel's own background, so it stops
    /// reading as a rule at all under this one cell. That is exactly what the centre tab strip's
    /// active tab already does with [`theme::surface::CENTER`]
    /// (`crate::work_surface::state::tab_colors`), one column over.
    ///
    /// A method on the model rather than an `if` in the renderer so the fact that these two
    /// colours are a *pair* - the cut and the rule it cuts - is stated once and can be asserted
    /// without a window.
    pub const fn rule_color(self) -> theme::ColorToken {
        if self.selected {
            theme::surface::RAIL
        } else {
            theme::border::RAIL_INNER
        }
    }

    /// The colour this cell's glyph rests at: [`theme::text::SELECTED`] for the view you are in,
    /// [`theme::text::FAINTER`] for one you are not (`Jerry.dc.html`'s own
    /// `fg: on ? '#dde2e7' : '#5e646a'`).
    pub const fn glyph_color(self) -> theme::ColorToken {
        if self.selected {
            theme::text::SELECTED
        } else {
            theme::text::FAINTER
        }
    }
}

/// The severities really present in the selected worktree's diagnostics, tallied over the real
/// list rather than authored as a pair.
///
/// `REVISION-2026-08-13.md` §2 is explicit about why all three are counted and not just two: "The
/// header names every severity the list is showing: an authored `{err: 2, warn: 2}` pair left the
/// `info` row uncounted and unnamed, so five diagnostics sat under a badge reading 4."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProblemTally {
    pub errors: usize,
    pub warnings: usize,
    pub hints: usize,
}

impl ProblemTally {
    /// Every diagnostic in the worktree, whatever its severity - the number the marker's tooltip
    /// reports.
    pub fn total(self) -> usize {
        self.errors + self.warnings + self.hints
    }

    /// The marker for this tally, or `None` when the worktree is clean.
    ///
    /// Red once anything is a real error, amber otherwise - `Jerry.dc.html`'s own
    /// `probTally.err ? '#e0625c' : '#e2a336'`. A worktree with only warnings is genuinely not in
    /// the same state as one that does not compile, and the strip is the only place that says so
    /// before you open the view.
    pub fn marker(self) -> Option<StripMarker> {
        StripMarker::new(
            self.total(),
            if self.errors > 0 {
                MarkerTone::Failure
            } else {
                MarkerTone::NeedsYou
            },
        )
    }

    /// The Problems body's own count line - `"2 errors · 2 warnings · 1 hint"`, naming only the
    /// severities really present.
    ///
    /// §2: "Both list headers and the Problems badge are **tallied over their own data**". Every
    /// term agrees through [`crate::root::plural`] rather than a hand-written ternary (§7 rule 9).
    /// Returns `None` for a clean worktree, where the empty note ([`problems_empty_note`]) says
    /// the real thing instead of a line of three zeroes.
    pub fn count_line(self) -> Option<String> {
        let parts: Vec<String> = [
            (self.errors, "error"),
            (self.warnings, "warning"),
            (self.hints, "hint"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, noun)| plural::count(count, noun, None))
        .collect();
        (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
    }
}

/// The Problems view's empty note, naming the checkout it is empty *for*.
///
/// `REVISION-2026-08-14.md` §6, verbatim: "A clean worktree gets *No diagnostics in `<branch>`.*
/// and no badge." Naming the branch is the point - §6's whole entry is that "Problems is keyed by
/// worktree and filtered on the active one, exactly like history - a diagnostic belongs to a
/// checkout", and a note that did not say which checkout would leave that unsaid at exactly the
/// moment it matters.
pub fn problems_empty_note(branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("No diagnostics in {branch}."),
        // A detached `HEAD` has no branch name to print, and inventing one would be the exact
        // "claimed more than it knew" defect §4g is about.
        None => "No diagnostics in this worktree.".to_string(),
    }
}

/// The Problems view's note when the worktree really does have diagnostics but the sidebar's own
/// filter box has hidden every one of them.
///
/// A different fact from [`problems_empty_note`]'s clean checkout, and said as one: the rail's own
/// list already distinguishes "this repo has no worktrees" from "your filter matched none of
/// them", and a Problems view that reported a clean checkout over five hidden rows would be
/// claiming more than it knows (`STAGE-A-CHANGELOG.md` §4g). Counts through
/// [`crate::root::plural`] like every other count in the window (§7 rule 9).
pub fn problems_filtered_away_note(hidden: usize) -> String {
    format!(
        "No match in this worktree's {}.",
        plural::count(hidden, "diagnostic", None)
    )
}

/// The cells the strip really paints, in order - **and the empty gate**.
///
/// `has_worktrees` is whether this window really has a worktree to look at. At `false` this
/// returns no cells at all, which is §1's "with no worktrees there are no views to offer, and a
/// switcher with four dead views is worse than no switcher", gated at the source: the markers
/// cannot survive the gate because they are built here, below it, rather than beside a `when(..)`
/// in the renderer that someone could later write for one and not the other.
///
/// `agents_needing_you` is the real count of agents waiting on a human. §1 names the unit
/// outright - "worktrees shows agents needing a human" - and `Jerry.dc.html` computes exactly
/// that (`sessions.filter(s => s.status === 'ask' || s.status === 'fail').length`), which is why
/// this is an agent count and not the worktree count §4v's one-line hue table reads as. It comes
/// from the same `crate::rail::state::urgency_counts` pass over the same [`crate::rail::state::
/// AgentRow`]s the title bar's own dots read, so the two can never report different numbers for
/// the same window. `problems` is the real tally for the selected worktree.
pub fn strip_view_cells(
    has_worktrees: bool,
    selected: SidebarView,
    agents_needing_you: usize,
    problems: ProblemTally,
) -> Vec<StripCell> {
    if !has_worktrees {
        return Vec::new();
    }
    SidebarView::ALL
        .iter()
        .map(|&view| StripCell {
            view,
            selected: view == selected,
            marker: match view {
                // Always amber, even when what is waiting is a failed agent: §4v's own
                // `badgeFg` table gives the Worktrees cell one hue, because this marker means
                // "there is work here waiting on you", not "how bad is it". The rail rows and
                // repo headers below it are where the two states are told apart - and §7 rule 4
                // ("two states distinguished anywhere in the app are never summed anywhere in
                // it") is not broken by the sum, because an agent is in exactly one `Status`:
                // adding the `Ask` and `Fail` counts cannot double-count one agent the way
                // summing two *worktree* counts could (§4q).
                SidebarView::Worktrees => {
                    StripMarker::new(agents_needing_you, MarkerTone::NeedsYou)
                }
                SidebarView::Problems => problems.marker(),
            },
        })
        .collect()
}

/// Which view the sidebar body really shows, given the empty gate.
///
/// With no worktrees there is no switcher, so there is no other view to be in: the body falls back
/// to Worktrees, which is where the rail paints its own empty state ("the sidebar shows the rail's
/// own empty state", §1). Without this, switching to Problems and then closing the last worktree
/// would strand the window on a Problems view with no cell to switch back from.
pub fn effective_view(has_worktrees: bool, selected: SidebarView) -> SidebarView {
    if has_worktrees {
        selected
    } else {
        SidebarView::Worktrees
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn views(cells: &[StripCell]) -> Vec<SidebarView> {
        cells.iter().map(|cell| cell.view).collect()
    }

    /// §4u/§4v: the strip is down to two view cells and the overflow. Search went to the right
    /// panel and History into the `⋯`, so neither may reappear as a cell here.
    #[test]
    fn the_strip_offers_worktrees_and_problems_and_nothing_else() {
        let cells = strip_view_cells(true, SidebarView::Worktrees, 0, ProblemTally::default());
        assert_eq!(
            views(&cells),
            vec![SidebarView::Worktrees, SidebarView::Problems]
        );
        assert_eq!(
            SidebarView::ALL.len(),
            2,
            "History belongs to the overflow (\u{a7}4u) and Search to the right panel (\u{a7}4u); \
             a third cell here would be re-adding a strip the design cut down"
        );
    }

    /// §1's empty gate, at the source. The assertion that matters is not just "no cells" - it is
    /// that a real, non-zero set of counts cannot smuggle a marker through, which is the exact
    /// defect the section describes ("ungated they claimed 3 agents needing a human and 4 problems
    /// on a day the rail, title bar and footer all reported zero").
    #[test]
    fn an_empty_day_offers_no_views_and_no_markers_whatever_the_counts_say() {
        let loud = ProblemTally {
            errors: 4,
            warnings: 2,
            hints: 1,
        };
        let cells = strip_view_cells(false, SidebarView::Problems, 3, loud);
        assert!(
            cells.is_empty(),
            "with no worktrees there are no views to offer"
        );
        assert_eq!(
            effective_view(false, SidebarView::Problems),
            SidebarView::Worktrees,
            "and the body falls back to the rail's own empty state rather than stranding the \
             window on a view with no cell to leave it by"
        );
        assert_eq!(
            effective_view(true, SidebarView::Problems),
            SidebarView::Problems
        );
    }

    /// Exactly one cell is selected, and it is the one asked for.
    #[test]
    fn exactly_one_cell_is_selected_and_it_is_the_current_view() {
        for view in SidebarView::ALL.iter().copied() {
            let cells = strip_view_cells(true, view, 0, ProblemTally::default());
            let selected: Vec<SidebarView> = cells
                .iter()
                .filter(|cell| cell.selected)
                .map(|cell| cell.view)
                .collect();
            assert_eq!(
                selected,
                vec![view],
                "two filled slabs would be two claims about which panel the rule joins"
            );
        }
    }

    /// A marker is a real count or it is not painted - §1's "Hidden at zero", carried by the
    /// constructor so no call site can forget it.
    #[test]
    fn a_marker_never_stands_for_nothing() {
        assert_eq!(StripMarker::new(0, MarkerTone::NeedsYou), None);
        assert_eq!(ProblemTally::default().marker(), None);
        let cells = strip_view_cells(true, SidebarView::Worktrees, 0, ProblemTally::default());
        assert!(cells.iter().all(|cell| cell.marker.is_none()));
    }

    /// `Jerry.dc.html`'s `probTally.err ? '#e0625c' : '#e2a336'`, and the Worktrees cell's single
    /// amber - read through the real tokens, so a theme rename can't quietly decouple them.
    #[test]
    fn the_marker_hues_are_the_apps_own_status_hues() {
        let warnings_only = ProblemTally {
            warnings: 2,
            hints: 1,
            ..ProblemTally::default()
        };
        assert_eq!(
            warnings_only.marker().expect("three diagnostics").tone,
            MarkerTone::NeedsYou
        );
        let with_errors = ProblemTally {
            errors: 1,
            ..warnings_only
        };
        assert_eq!(
            with_errors.marker().expect("four diagnostics").tone,
            MarkerTone::Failure
        );
        assert_eq!(MarkerTone::NeedsYou.color(), theme::status::ASK);
        assert_eq!(MarkerTone::Failure.color(), theme::status::FAIL);

        let cells = strip_view_cells(true, SidebarView::Worktrees, 2, with_errors);
        let worktrees = cells[0].marker.expect("two agents need a human");
        assert_eq!(
            (worktrees.count, worktrees.tone),
            (2, MarkerTone::NeedsYou),
            "\u{a7}4v's own badge table gives the Worktrees cell one hue - the rail rows below it \
             are where amber and red are told apart"
        );
    }

    /// §1's `title="<view> — <hint>"`, with §4v's count moved into it.
    #[test]
    fn every_cell_carries_its_view_hint_tooltip_and_the_count_lives_in_it() {
        assert_eq!(
            SidebarView::Worktrees.tooltip(None),
            "Worktrees \u{2014} repo \u{b7} worktree \u{b7} agent"
        );
        assert_eq!(
            SidebarView::Problems.tooltip(StripMarker::new(5, MarkerTone::Failure)),
            "Problems \u{2014} diagnostics in this worktree (5)"
        );
        for view in SidebarView::ALL.iter().copied() {
            let tip = view.tooltip(None);
            assert!(
                tip.starts_with(view.label()) && tip.contains(" \u{2014} "),
                "{tip:?} is not `<view> \u{2014} <hint>`"
            );
        }
    }

    /// §4v's cut-out, as the pair it is: the selected cell paints the column rule in the rail's
    /// own background - the same trick the centre tab strip's active tab plays with the work
    /// surface's - and every other cell paints the real rule.
    #[test]
    fn the_selected_cell_paints_the_column_rule_in_the_panels_own_background() {
        let cells = strip_view_cells(true, SidebarView::Problems, 0, ProblemTally::default());
        let problems = cells
            .iter()
            .find(|cell| cell.view == SidebarView::Problems)
            .expect("the Problems cell");
        let worktrees = cells
            .iter()
            .find(|cell| cell.view == SidebarView::Worktrees)
            .expect("the Worktrees cell");

        assert_eq!(
            problems.rule_color(),
            theme::surface::RAIL,
            "the selected cell's rule is the rail's own background - that is what 'cuts' it"
        );
        assert_eq!(
            worktrees.rule_color(),
            theme::border::RAIL_INNER,
            "and every other cell draws the real column rule, in the one colour all three column \
             headers share"
        );
        assert_ne!(
            problems.rule_color(),
            worktrees.rule_color(),
            "a cut that painted the same colour as the rule would not be a cut"
        );
        assert_eq!(problems.glyph_color(), theme::text::SELECTED);
        assert_eq!(worktrees.glyph_color(), theme::text::FAINTER);
    }

    /// §8's mapping table, held here too so the strip's own glyph choice can't drift from it.
    #[test]
    fn the_view_glyphs_are_the_two_the_revision_maps() {
        assert_eq!(SidebarView::Worktrees.icon(), Icon::TreeStructure);
        assert_eq!(SidebarView::Problems.icon(), Icon::Warning);
    }

    /// §2's "the header names every severity the list is showing" - and only those, so a
    /// warnings-only worktree does not print `0 errors`.
    #[test]
    fn the_count_line_names_every_severity_present_and_no_others() {
        assert_eq!(
            ProblemTally {
                errors: 2,
                warnings: 2,
                hints: 1
            }
            .count_line()
            .as_deref(),
            Some("2 errors \u{b7} 2 warnings \u{b7} 1 hint")
        );
        assert_eq!(
            ProblemTally {
                warnings: 1,
                ..ProblemTally::default()
            }
            .count_line()
            .as_deref(),
            Some("1 warning"),
            "\u{a7}7 rule 9: singular through the helper, never `1 warnings`"
        );
        assert_eq!(
            ProblemTally::default().count_line(),
            None,
            "a clean worktree gets the empty note, not a line of zeroes"
        );
    }

    /// A filter that hid every row is not a clean checkout, and must not claim to be one.
    #[test]
    fn a_filtered_away_list_says_so_rather_than_reporting_a_clean_checkout() {
        assert_eq!(
            problems_filtered_away_note(5),
            "No match in this worktree's 5 diagnostics."
        );
        assert_eq!(
            problems_filtered_away_note(1),
            "No match in this worktree's 1 diagnostic.",
            "\u{a7}7 rule 9: through the helper, never `1 diagnostics`"
        );
        assert_ne!(
            problems_filtered_away_note(3),
            problems_empty_note(Some("main")),
            "the two empty states are two different facts and must read as such"
        );
    }

    /// §6's "A clean worktree gets *No diagnostics in `<branch>`.*" - and an honest sentence on a
    /// detached `HEAD`, where there is no branch name to print.
    #[test]
    fn the_empty_note_names_the_checkout_it_is_empty_for() {
        assert_eq!(
            problems_empty_note(Some("fix/auth")),
            "No diagnostics in fix/auth."
        );
        assert_eq!(
            problems_empty_note(None),
            "No diagnostics in this worktree.",
            "a detached HEAD has no branch name, and inventing one would claim more than the \
             view knows"
        );
    }
}
