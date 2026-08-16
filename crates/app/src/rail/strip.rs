//! The sidebar strip, as pure data (GitHub issue #291): which views the left column offers, which
//! cells the strip really paints, what each cell's marker and tooltip say, and the one gate that
//! empties the strip on a day with no worktrees.

use crate::icons::Icon;
use crate::lsp::diagnostics::Severity;
use crate::root::plural;
use crate::theme;

/// A view the left column can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SidebarView {
    /// The repo → worktree → agent tree.
    #[default]
    Worktrees,
    /// The selected worktree's LSP diagnostics.
    Problems,
    /// Agent history, keyed repo → worktree → run (GitHub issue #227). Reached from the `⋯`
    /// overflow rather than from a cell - see this enum's own docs.
    History,
}

impl SidebarView {
    /// Every view **the strip paints a cell for**, in the order it paints them - which is
    /// deliberately not every variant of this enum. See [`SidebarView`]'s own docs.
    pub const ALL: &'static [SidebarView] = &[SidebarView::Worktrees, SidebarView::Problems];

    /// The view's name - the first half of its `"<view> — <hint>"` tooltip, and the word the
    /// Problems body's own copy uses.
    pub const fn label(self) -> &'static str {
        match self {
            SidebarView::Worktrees => "Worktrees",
            SidebarView::Problems => "Problems",
            SidebarView::History => "History",
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
            // The wording `crate::rail::menu::overflow_menu_groups`' own History row already
            // carries, so the row you reach this view by and the view itself say one thing.
            SidebarView::History => "earlier runs, by repo and worktree",
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
            // §8's own third mapping, and §4u's "with the glyphs they had in the strip (clock,
            // sliders) so the move out of the strip does not cost their recognisability" - so the
            // overflow row and this view name the same mark even though no cell paints it.
            SidebarView::History => Icon::ClockCounterClockwise,
        }
    }

    /// The cell's `title="<view> — <hint>"` tooltip (§1), with the marker's real count appended in
    /// parentheses when there is one - `Jerry.dc.html`'s own
    /// `v.label + ' — ' + v.hint + (badge ? ' (' + badge + ')' : '')`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripMarker {
    /// How many things the marker is standing for. Shown in the tooltip, never on the cell.
    pub count: usize,
    pub tone: MarkerTone,
}

impl StripMarker {
    /// A marker for `count` things, or `None` at zero.
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

/// One diagnostic in the Problems list, already reduced to what the row paints and what a click
/// on it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub severity: Severity,
    pub message: String,
    /// The path, relative to the worktree root - every row in this list is inside the checkout by
    /// construction (see `crate::rail::strip_render::AdeApp::worktree_problems`), and a Problems
    /// list showing 60 characters of shared prefix on every row says nothing per row.
    pub file: String,
    /// The real absolute path the click opens. Kept beside [`Self::file`] rather than re-joined
    /// from it at click time: re-deriving a path from a *display* string is how a row that shows
    /// one file opens another.
    pub path: std::path::PathBuf,
    /// 1-based, the way every compiler and every other row in this app prints - and the way
    /// `crate::code_surface::lsp_ui::AdeApp::open_file_at_line` takes it. LSP's own positions are
    /// 0-based; the conversion happens once, where the diagnostic is read, rather than at each of
    /// the two use sites.
    pub line: u32,
    pub column: u32,
    /// The server that reported it (`rustc`, `clippy`, `rust-analyzer`), when it named itself.
    pub source: Option<String>,
}

impl Problem {
    /// `line:column`, the way `Jerry.dc.html`'s own `p.line` prints it (`212:17`).
    pub fn position(&self) -> String {
        format!("{}:{}", self.line, self.column)
    }

    /// Whether this row survives the sidebar's own filter box - the same case-insensitive
    /// substring test over the row's own visible text that [`crate::rail::state::WorktreeRow::
    /// matches_filter`] applies to a worktree row, so one field behaves the same way whichever
    /// view is under it. A blank query matches everything.
    pub fn matches_filter(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        self.message.to_lowercase().contains(&query)
            || self.file.to_lowercase().contains(&query)
            || self
                .source
                .as_deref()
                .is_some_and(|source| source.to_lowercase().contains(&query))
    }

    /// Most severe first, then by file, then by position - the list-level counterpart of
    /// [`Severity::rank`]'s own within-a-line "worst wins" ordering.
    pub fn worst_first(left: &Problem, right: &Problem) -> std::cmp::Ordering {
        right
            .severity
            .rank()
            .cmp(&left.severity.rank())
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| (left.line, left.column).cmp(&(right.line, right.column)))
    }
}

/// The severities really present in the selected worktree's diagnostics, tallied over the real
/// list rather than authored as a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProblemTally {
    pub errors: usize,
    pub warnings: usize,
    pub hints: usize,
}

impl ProblemTally {
    /// `problems` counted into `REVISION-2026-08-13.md` §2's three buckets.
    pub fn over(problems: &[Problem]) -> Self {
        let mut tally = ProblemTally::default();
        for problem in problems {
            match problem.severity {
                Severity::Error => tally.errors += 1,
                Severity::Warning => tally.warnings += 1,
                Severity::Information | Severity::Hint => tally.hints += 1,
            }
        }
        tally
    }

    /// Every diagnostic in the worktree, whatever its severity - the number the marker's tooltip
    /// reports.
    pub fn total(self) -> usize {
        self.errors + self.warnings + self.hints
    }

    /// The marker for this tally, or `None` when the worktree is clean.
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
pub fn problems_filtered_away_note(hidden: usize) -> String {
    format!(
        "No match in this worktree's {}.",
        plural::count(hidden, "diagnostic", None)
    )
}

/// The cells the strip really paints, in order - **and the empty gate**.
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
                // Unreachable: this maps over `ALL`, which History is deliberately not in. Stated
                // as `None` rather than as an `unreachable!()` so that the day History *does* get
                // a cell, the compiler asks for its marker here instead of the app panicking.
                SidebarView::History => None,
            },
        })
        .collect()
}

/// Which view the sidebar body really shows, given the empty gate.
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

    fn problem(severity: Severity, file: &str, line: u32, message: &str, source: &str) -> Problem {
        Problem {
            severity,
            message: message.to_string(),
            file: file.to_string(),
            path: std::path::PathBuf::from("/wt").join(file),
            line,
            column: 9,
            source: Some(source.to_string()),
        }
    }

    #[test]
    fn a_filter_narrows_the_list_without_touching_what_the_strip_reports() {
        let rows = vec![
            problem(
                Severity::Error,
                "src/auth/session.rs",
                212,
                "cannot borrow `self.tokens` as mutable",
                "rustc",
            ),
            problem(
                Severity::Warning,
                "tests/auth_race.rs",
                44,
                "unused variable: `barrier`",
                "clippy",
            ),
        ];

        assert_eq!(
            rows.iter().filter(|row| row.matches_filter("")).count(),
            2,
            "a blank query matches everything, exactly as it does for a worktree row"
        );
        // By message, by path, and by the server that reported it - all three of the row's own
        // visible fields.
        assert_eq!(
            rows.iter()
                .filter(|row| row.matches_filter("BORROW"))
                .count(),
            1,
            "case-insensitive, over the message"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.matches_filter("tests/"))
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.matches_filter("clippy"))
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.matches_filter("nothing"))
                .count(),
            0
        );

        assert_eq!(
            ProblemTally::over(&rows),
            ProblemTally {
                errors: 1,
                warnings: 1,
                hints: 0
            },
            "the tally the strip's marker reads is over the whole worktree, unfiltered"
        );
    }

    #[test]
    fn a_rows_position_reads_the_way_every_compiler_prints_one() {
        let row = problem(Severity::Error, "src/db/mod.rs", 118, "no method", "rustc");
        assert_eq!(row.position(), "118:9");
    }

    #[test]
    fn the_list_orders_worst_first_and_never_leaves_two_rows_tied() {
        let mut rows = [
            problem(Severity::Hint, "src/a.rs", 4, "hint", "clippy"),
            problem(Severity::Warning, "src/z.rs", 1, "warning", "clippy"),
            problem(Severity::Error, "src/z.rs", 9, "second error", "rustc"),
            problem(Severity::Error, "src/z.rs", 2, "first error", "rustc"),
            problem(Severity::Error, "src/a.rs", 7, "earlier file", "rustc"),
            problem(Severity::Information, "src/a.rs", 3, "info", "clippy"),
        ];
        rows.sort_by(Problem::worst_first);
        assert_eq!(
            rows.iter()
                .map(|row| (row.file.as_str(), row.line))
                .collect::<Vec<_>>(),
            vec![
                ("src/a.rs", 7),
                ("src/z.rs", 2),
                ("src/z.rs", 9),
                ("src/z.rs", 1),
                ("src/a.rs", 3),
                ("src/a.rs", 4),
            ]
        );
        assert!(
            rows.windows(2)
                .all(|pair| Problem::worst_first(&pair[0], &pair[1]) != std::cmp::Ordering::Equal),
            "an ordering that left two rows equal would let a re-render reshuffle them under the \
             pointer"
        );
    }

    #[test]
    fn the_tally_counts_every_row_the_list_is_showing() {
        let rows = vec![
            problem(Severity::Error, "a.rs", 1, "one", "rustc"),
            problem(Severity::Error, "b.rs", 1, "two", "rustc"),
            problem(Severity::Warning, "c.rs", 1, "three", "clippy"),
            problem(Severity::Warning, "d.rs", 1, "four", "clippy"),
            problem(Severity::Information, "e.rs", 1, "five", "clippy"),
        ];
        let tally = ProblemTally::over(&rows);
        assert_eq!(
            tally,
            ProblemTally {
                errors: 2,
                warnings: 2,
                hints: 1
            }
        );
        assert_eq!(
            tally.total(),
            rows.len(),
            "\u{a7}2's own defect: an authored {{err: 2, warn: 2}} pair left the info row \
             uncounted, so five diagnostics sat under a badge reading 4"
        );
        assert_eq!(
            tally.count_line().as_deref(),
            Some("2 errors \u{b7} 2 warnings \u{b7} 1 hint")
        );
    }

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
        assert!(
            !SidebarView::ALL.contains(&SidebarView::History),
            "History is a real view the body paints, reached from the \u{2ef} overflow - it must \
             never acquire a cell"
        );
    }

    #[test]
    fn history_is_a_full_view_even_though_it_has_no_cell() {
        assert_eq!(SidebarView::History.label(), "History");
        assert_eq!(
            SidebarView::History.tooltip(None),
            "History \u{2014} earlier runs, by repo and worktree"
        );
        assert_eq!(
            SidebarView::History.icon(),
            Icon::ClockCounterClockwise,
            "\u{a7}4u: the overflow keeps the glyph History had in the strip"
        );
        assert_eq!(
            effective_view(true, SidebarView::History),
            SidebarView::History
        );
        assert_eq!(
            effective_view(false, SidebarView::History),
            SidebarView::Worktrees,
            "an empty day has no runs to index either, and no cell to switch back from"
        );
    }

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

    #[test]
    fn a_marker_never_stands_for_nothing() {
        assert_eq!(StripMarker::new(0, MarkerTone::NeedsYou), None);
        assert_eq!(ProblemTally::default().marker(), None);
        let cells = strip_view_cells(true, SidebarView::Worktrees, 0, ProblemTally::default());
        assert!(cells.iter().all(|cell| cell.marker.is_none()));
    }

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

    #[test]
    fn the_view_glyphs_are_the_two_the_revision_maps() {
        assert_eq!(SidebarView::Worktrees.icon(), Icon::TreeStructure);
        assert_eq!(SidebarView::Problems.icon(), Icon::Warning);
    }

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
