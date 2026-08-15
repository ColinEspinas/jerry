//! The Search panel's own state, and the pure decisions the panel's chrome reads off it: which
//! of the three design states the body is in, what the count row says, and what each of the four
//! real inputs holds.
//!
//! GPUI-free apart from the four [`gpui::FocusHandle`]s the fields need - which are plain data
//! (`FocusHandle` is a cheap, cloneable id, not a window) - so every rule below is a real unit
//! test rather than a claim checked only by looking at a screenshot.
//!
//! ## Three states, gated on one flag
//!
//! `REVISION-2026-08-14.md` §5's table **is** the spec, and [`BodyState`] is it as an enum:
//!
//! | | count row | body | fold-all · Replace all |
//! |---|---|---|---|
//! | no query | *(empty)* | `Search the files in <branch>.` | hidden |
//! | no match | `no results` | `No matches for "<q>" in <branch>.` | hidden |
//! | results | `14 results in 6 files` | the tree | shown |
//!
//! §4w's own account of why this is three and not two is the whole reason the flag exists:
//! "making the query a real input created a state that could not previously exist - an empty
//! field - and every derived value still branched on the match count alone, so *not searched yet*
//! rendered as `no results` ... That asserts a fact about the worktree from a search nobody ran."
//!
//! Two further states the design table does not have, because a mock cannot have them: a search
//! genuinely **in flight**, and a regex that does not **compile**. Both are real, and both are
//! shown as themselves rather than folded into `no results` - which would be the same lie the
//! table exists to stop, one layer down. In particular the results of a *previous* query are
//! never left on screen under a newer one: [`SearchPanel::body_state`] compares the outcome's own
//! query against what is typed now, so a stale tree reports itself as still searching instead of
//! answering a question nobody asked.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui::FocusHandle;

use crate::root::plural;
use crate::search::engine::{SearchOptions, SearchOutcome};
use crate::text_history::TextField;

/// Which of the four real inputs the panel is typing into.
///
/// A single enum rather than four booleans or four separate key handlers: the four fields share
/// one keymap and one key-down handler, and "which one is focused" is exactly one fact. Four
/// flags would be four facts that can disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchField {
    Query,
    Replace,
    Include,
    Exclude,
}

impl SearchField {
    /// Every field, in the order Tab walks them - which is also the order they are stacked on
    /// screen.
    pub const ALL: [SearchField; 4] = [
        SearchField::Query,
        SearchField::Replace,
        SearchField::Include,
        SearchField::Exclude,
    ];
}

/// One of the three modifier buttons, so the row can be built by iteration rather than three
/// near-identical hand-written blocks that can drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchModifier {
    /// `Aa`.
    MatchCase,
    /// `ab`, drawn underlined the way VS Code draws it (`STAGE-A-CHANGELOG.md` §4v).
    WholeWord,
    /// `.*`.
    Regex,
}

impl SearchModifier {
    pub const ALL: [SearchModifier; 3] = [
        SearchModifier::MatchCase,
        SearchModifier::WholeWord,
        SearchModifier::Regex,
    ];

    /// The glyph pair `REVISION-2026-08-14.md` §5 names.
    pub fn label(self) -> &'static str {
        match self {
            SearchModifier::MatchCase => "Aa",
            SearchModifier::WholeWord => "ab",
            SearchModifier::Regex => ".*",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            SearchModifier::MatchCase => "Match case",
            SearchModifier::WholeWord => "Match whole word",
            SearchModifier::Regex => "Use regular expression",
        }
    }

    pub fn is_on(self, options: SearchOptions) -> bool {
        match self {
            SearchModifier::MatchCase => options.match_case,
            SearchModifier::WholeWord => options.whole_word,
            SearchModifier::Regex => options.regex,
        }
    }

    pub fn toggle(self, options: &mut SearchOptions) {
        match self {
            SearchModifier::MatchCase => options.match_case = !options.match_case,
            SearchModifier::WholeWord => options.whole_word = !options.whole_word,
            SearchModifier::Regex => options.regex = !options.regex,
        }
    }
}

/// A completed search, kept beside the exact query and options that produced it.
///
/// The query is stored rather than assumed: it is what lets [`SearchPanel::body_state`] tell "these
/// are the results for what is typed" from "these are the results for what *was* typed", which is
/// the difference between a result tree and a stale one.
#[derive(Debug, Clone)]
pub struct CompletedSearch {
    pub query: String,
    pub options: SearchOptions,
    pub include: String,
    pub exclude: String,
    pub outcome: SearchOutcome,
}

impl CompletedSearch {
    /// Whether this really answers the panel's current inputs.
    fn answers(&self, panel: &SearchPanel) -> bool {
        self.query == panel.query.as_str()
            && self.options == panel.options
            && self.include == panel.include.as_str()
            && self.exclude == panel.exclude.as_str()
    }
}

/// What the panel's body is showing right now - `REVISION-2026-08-14.md` §5's table, plus the two
/// states a static mock cannot have. See this module's own docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyState {
    /// Nothing typed. Not the same fact as "searched, found nothing".
    NotSearched,
    /// The query is a regular expression that does not compile, with the message from `regex`
    /// itself - the only thing that can say which character is the problem.
    InvalidQuery(String),
    /// A real search is running, or the last completed one answers a different query.
    Searching,
    /// Searched, and the worktree really holds nothing matching.
    NoMatch,
    /// Searched, and there are hits.
    Results,
}

impl BodyState {
    /// Whether the fold-all caret and `Replace all` exist at all - `REVISION-2026-08-14.md` §7
    /// rule 2: "A control that acts on results does not exist when there are none."
    pub fn has_results(&self) -> bool {
        matches!(self, BodyState::Results)
    }
}

/// The whole Search tab's state.
pub struct SearchPanel {
    /// The four real inputs. `REVISION-2026-08-14.md` §5: "Four real inputs: query, replace,
    /// include, exclude. A fake field directly below a real one is a dead end the user will
    /// click."
    pub query: TextField,
    pub replace: TextField,
    pub include: TextField,
    pub exclude: TextField,
    /// Which one the key handler is typing into. See [`SearchField`].
    pub focused_field: SearchField,
    pub options: SearchOptions,
    /// Whether the `⇄` replace row is revealed.
    pub replace_open: bool,
    /// Whether the funnel's include/exclude rows are revealed.
    pub globs_open: bool,
    /// Files whose match rows are collapsed - absent means expanded, so a fresh search opens
    /// everything, which is what the design's own default state draws.
    pub collapsed: HashSet<PathBuf>,
    /// The last search that really finished.
    pub completed: Option<CompletedSearch>,
    /// A real search is in flight right now.
    pub searching: bool,
    /// The query could not be compiled - `Some` only while that is still true of what is typed.
    pub error: Option<String>,
    /// Bumped for every search started; a finishing task whose generation is stale is discarded,
    /// so a slow search over a big worktree can never overwrite a newer, faster one's results.
    pub generation: u64,
    /// What the last replace really did, said out loud under the query row until the next edit.
    pub notice: Option<String>,
    /// One handle per field - a shared handle could not tell the renderer which field's caret to
    /// paint, and painting all four at once is exactly the bug class GitHub issue #45 keeps
    /// finding.
    pub query_focus_handle: FocusHandle,
    pub replace_focus_handle: FocusHandle,
    pub include_focus_handle: FocusHandle,
    pub exclude_focus_handle: FocusHandle,
}

impl SearchPanel {
    pub fn new(cx: &mut gpui::App) -> Self {
        SearchPanel {
            query: TextField::new(),
            replace: TextField::new(),
            include: TextField::new(),
            exclude: TextField::new(),
            focused_field: SearchField::Query,
            options: SearchOptions::default(),
            replace_open: false,
            globs_open: false,
            collapsed: HashSet::new(),
            completed: None,
            searching: false,
            error: None,
            generation: 0,
            notice: None,
            query_focus_handle: cx.focus_handle(),
            replace_focus_handle: cx.focus_handle(),
            include_focus_handle: cx.focus_handle(),
            exclude_focus_handle: cx.focus_handle(),
        }
    }

    pub fn field(&self, which: SearchField) -> &TextField {
        match which {
            SearchField::Query => &self.query,
            SearchField::Replace => &self.replace,
            SearchField::Include => &self.include,
            SearchField::Exclude => &self.exclude,
        }
    }

    pub fn field_mut(&mut self, which: SearchField) -> &mut TextField {
        match which {
            SearchField::Query => &mut self.query,
            SearchField::Replace => &mut self.replace,
            SearchField::Include => &mut self.include,
            SearchField::Exclude => &mut self.exclude,
        }
    }

    pub fn focus_handle(&self, which: SearchField) -> &FocusHandle {
        match which {
            SearchField::Query => &self.query_focus_handle,
            SearchField::Replace => &self.replace_focus_handle,
            SearchField::Include => &self.include_focus_handle,
            SearchField::Exclude => &self.exclude_focus_handle,
        }
    }

    /// Whether a field is currently reachable - a hidden row's field is not something Tab may land
    /// on, and `⇄`/the funnel are what reveal them.
    pub fn field_is_visible(&self, which: SearchField) -> bool {
        match which {
            SearchField::Query => true,
            SearchField::Replace => self.replace_open,
            SearchField::Include | SearchField::Exclude => self.globs_open,
        }
    }

    /// The next visible field Tab should move to, wrapping. `None` only if somehow nothing is
    /// visible, which cannot happen - the query row is always there.
    pub fn next_visible_field(&self, from: SearchField) -> Option<SearchField> {
        let start = SearchField::ALL
            .iter()
            .position(|field| *field == from)
            .unwrap_or(0);
        (1..=SearchField::ALL.len())
            .map(|step| SearchField::ALL[(start + step) % SearchField::ALL.len()])
            .find(|field| self.field_is_visible(*field))
    }

    /// The query as the search would run it. `REVISION-2026-08-14.md` §5's own `hasQ` is
    /// `findQ.trim().length > 0`, but this app does **not** trim: a whitespace query is a real
    /// search here (see `crate::search::engine::Matcher::compile`), so the has-query flag is
    /// simply "not empty".
    pub fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    /// The one three-state gate the count row, the body and the results-only controls all read.
    pub fn body_state(&self) -> BodyState {
        if !self.has_query() {
            return BodyState::NotSearched;
        }
        if let Some(error) = &self.error {
            return BodyState::InvalidQuery(error.clone());
        }
        match &self.completed {
            Some(completed) if completed.answers(self) => {
                if completed.outcome.total_matches == 0 {
                    BodyState::NoMatch
                } else {
                    BodyState::Results
                }
            }
            // Either nothing has finished yet, or what finished answers a different question.
            // Both are "we do not know yet", and saying so beats showing the previous query's
            // tree under this one.
            _ => BodyState::Searching,
        }
    }

    /// The outcome the tree should draw, or `None` in every state that is not [`BodyState::Results`].
    pub fn results(&self) -> Option<&SearchOutcome> {
        match self.body_state() {
            BodyState::Results => self.completed.as_ref().map(|completed| &completed.outcome),
            _ => None,
        }
    }

    /// The count row's text - `""`, `no results`, `14 results in 6 files`, or the two states the
    /// design table does not have.
    ///
    /// Every count goes through `crate::root::plural` (`REVISION-2026-08-14.md` §7 rule 9), so
    /// `1 result in 1 file` is never `1 results in 1 files`.
    pub fn count_label(&self) -> String {
        match self.body_state() {
            BodyState::NotSearched => String::new(),
            BodyState::InvalidQuery(_) => "invalid pattern".to_string(),
            BodyState::Searching => "searching\u{2026}".to_string(),
            BodyState::NoMatch => "no results".to_string(),
            BodyState::Results => {
                let Some(outcome) = self.results() else {
                    return String::new();
                };
                let label = format!(
                    "{} in {}",
                    plural::count(outcome.total_matches, "result", None),
                    plural::count(outcome.files.len(), "file", None)
                );
                if outcome.truncated {
                    // The issue's own "results cap with an honest truncation notice". Said in the
                    // count row itself rather than only in a tooltip: the number beside it is a
                    // floor, not a total, and a reader who does not hover would otherwise take it
                    // for the total.
                    format!("{label} (capped)")
                } else {
                    label
                }
            }
        }
    }

    /// The count row's tooltip while the cap was hit, which is where the number the label cannot
    /// fit belongs.
    pub fn truncation_tooltip(&self) -> Option<String> {
        let outcome = self.results()?;
        outcome.truncated.then(|| {
            format!(
                "Stopped at {} across {} - narrow the query, or use the path filters",
                plural::count(outcome.total_matches, "match", Some("matches")),
                plural::count(outcome.scanned_files, "file", None)
            )
        })
    }

    /// The body's message in every state that is a message rather than a tree.
    ///
    /// `branch` is the active worktree's branch, exactly as the design's two sentences name it -
    /// `Search the files in <branch>.` and `No matches for "<q>" in <branch>.`
    pub fn body_message(&self, branch: &str) -> Option<String> {
        match self.body_state() {
            BodyState::NotSearched => Some(format!("Search the files in {branch}.")),
            BodyState::Searching => Some(format!("Searching {branch}\u{2026}")),
            BodyState::NoMatch => Some(format!(
                "No matches for \u{201c}{}\u{201d} in {branch}.",
                self.query.as_str()
            )),
            BodyState::InvalidQuery(error) => Some(error),
            BodyState::Results => None,
        }
    }

    /// `Replace all`'s tooltip, derived from live hits.
    ///
    /// The issue calls out the mock's own bug here by name: it "had `14` hardcoded and would have
    /// lied on the first keystroke". Returns `None` in every state where there is nothing to
    /// replace, which is the same gate the button itself is behind.
    pub fn replace_all_tooltip(&self) -> Option<String> {
        let outcome = self.results()?;
        Some(format!(
            "Replace all {} in {}",
            plural::count(outcome.total_matches, "match", Some("matches")),
            plural::count(outcome.files.len(), "file", None)
        ))
    }

    /// Whether every file in the current results is collapsed - what the fold-all caret points
    /// at, and what clicking it inverts. `REVISION-2026-08-14.md` §5: fold-all is "the same
    /// `▾`/`▸` caret a file row uses, since it is the same action applied to all of them".
    pub fn all_collapsed(&self) -> bool {
        match self.results() {
            Some(outcome) => outcome
                .files
                .iter()
                .all(|file| self.collapsed.contains(&file.path)),
            None => false,
        }
    }

    /// Collapses every result file, or expands every one of them - whichever the caret is
    /// currently offering.
    pub fn toggle_fold_all(&mut self) {
        let collapse = !self.all_collapsed();
        let Some(paths) = self.results().map(|outcome| {
            outcome
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        for path in paths {
            if collapse {
                self.collapsed.insert(path);
            } else {
                self.collapsed.remove(&path);
            }
        }
    }

    /// Every result file's absolute path, for a Replace all.
    pub fn result_paths(&self) -> Vec<PathBuf> {
        self.results()
            .map(|outcome| outcome.files.iter().map(|file| file.path.clone()).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::engine::{FileMatches, LineMatch};
    use std::time::Instant;

    /// A panel with no `gpui::App` behind it. The four focus handles are the only thing `new`
    /// needs a context for, and a test of the three-state gate has no business standing a window
    /// up for them - so this builds the same struct with handles from a real, headless
    /// `gpui::App` provided by the caller.
    fn panel(cx: &mut gpui::App) -> SearchPanel {
        SearchPanel::new(cx)
    }

    fn outcome(files: Vec<(&str, usize)>) -> SearchOutcome {
        let files: Vec<FileMatches> = files
            .into_iter()
            .map(|(relative, hits)| FileMatches {
                path: PathBuf::from("/wt").join(relative),
                relative: relative.to_string(),
                lines: (0..hits)
                    .map(|index| LineMatch {
                        line_number: index + 1,
                        text: "hit".to_string(),
                        // `std::iter::once` rather than `vec![0..3]`: clippy reads a one-element
                        // `Vec<Range>` literal as a probable mistyped `(0..3).collect()`, which it
                        // is not.
                        ranges: std::iter::once(0..3).collect(),
                    })
                    .collect(),
            })
            .collect();
        let total_matches = files.iter().map(|file| file.match_count()).sum();
        SearchOutcome {
            files,
            total_matches,
            truncated: false,
            scanned_files: 12,
        }
    }

    /// Marks `panel`'s current inputs as answered by `outcome`.
    fn complete(panel: &mut SearchPanel, outcome: SearchOutcome) {
        panel.completed = Some(CompletedSearch {
            query: panel.query.as_str().to_string(),
            options: panel.options,
            include: panel.include.as_str().to_string(),
            exclude: panel.exclude.as_str().to_string(),
            outcome,
        });
        panel.searching = false;
    }

    #[gpui::test]
    fn an_empty_query_is_not_searched_yet_not_no_results(cx: &mut gpui::TestAppContext) {
        let panel = cx.update(panel);
        assert_eq!(panel.body_state(), BodyState::NotSearched);
        assert_eq!(
            panel.count_label(),
            "",
            "the count row is empty, not `no results` - a search nobody ran asserts nothing about \
             the worktree"
        );
        assert_eq!(
            panel.body_message("fix/auth-token-race").as_deref(),
            Some("Search the files in fix/auth-token-race.")
        );
        assert!(!panel.body_state().has_results());
        assert_eq!(panel.replace_all_tooltip(), None);
    }

    #[gpui::test]
    fn a_query_with_no_hits_is_a_different_state_with_a_different_sentence(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut panel = cx.update(panel);
        panel.query.set("refresh_token", Instant::now());
        complete(&mut panel, outcome(Vec::new()));
        assert_eq!(panel.body_state(), BodyState::NoMatch);
        assert_eq!(panel.count_label(), "no results");
        assert_eq!(
            panel.body_message("fix/auth-token-race").as_deref(),
            Some("No matches for \u{201c}refresh_token\u{201d} in fix/auth-token-race.")
        );
        assert!(
            !panel.body_state().has_results(),
            "a control that acts on results does not exist when there are none"
        );
    }

    #[gpui::test]
    fn results_read_through_the_pluralisation_helper_in_both_directions(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut panel = cx.update(panel);
        panel.query.set("refresh_token", Instant::now());
        complete(&mut panel, outcome(vec![("src/a.rs", 1)]));
        assert_eq!(
            panel.count_label(),
            "1 result in 1 file",
            "never `1 results in 1 files` - the exact defect §7 rule 9 exists for"
        );

        complete(
            &mut panel,
            outcome(vec![
                ("src/auth/session.rs", 4),
                ("src/auth/store.rs", 2),
                ("src/auth/mod.rs", 1),
                ("tests/auth_race.rs", 3),
                ("src/api/users.rs", 2),
                ("migrations/0031.sql", 2),
            ]),
        );
        assert_eq!(panel.count_label(), "14 results in 6 files");
        assert!(panel.body_state().has_results());
    }

    #[gpui::test]
    fn the_replace_all_tooltip_derives_from_live_hits_rather_than_a_hardcoded_number(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut panel = cx.update(panel);
        panel.query.set("refresh_token", Instant::now());
        complete(&mut panel, outcome(vec![("src/a.rs", 4), ("src/b.rs", 2)]));
        assert_eq!(
            panel.replace_all_tooltip().as_deref(),
            Some("Replace all 6 matches in 2 files")
        );

        // One keystroke later the results answer a different query - which is precisely where the
        // mock's own hardcoded `14` would have started lying.
        panel.query.insert_str("s", Instant::now());
        assert_eq!(panel.replace_all_tooltip(), None);
    }

    #[gpui::test]
    fn results_for_a_previous_query_are_never_shown_under_a_newer_one(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut panel = cx.update(panel);
        panel.query.set("refresh", Instant::now());
        complete(&mut panel, outcome(vec![("src/a.rs", 3)]));
        assert_eq!(panel.body_state(), BodyState::Results);

        panel.query.insert_str("_token", Instant::now());
        assert_eq!(
            panel.body_state(),
            BodyState::Searching,
            "a tree answering a question nobody asked is the same lie the three-state gate exists \
             to stop, one layer down"
        );
        assert_eq!(panel.results(), None);
    }

    #[gpui::test]
    fn changing_a_modifier_or_a_glob_also_invalidates_the_results(cx: &mut gpui::TestAppContext) {
        let mut panel = cx.update(panel);
        panel.query.set("token", Instant::now());
        complete(&mut panel, outcome(vec![("src/a.rs", 1)]));

        panel.options.match_case = true;
        assert_eq!(
            panel.body_state(),
            BodyState::Searching,
            "`Aa` changes results - the acceptance criterion says so in as many words"
        );

        panel.options.match_case = false;
        assert_eq!(panel.body_state(), BodyState::Results);
        panel.include.set("src/**", Instant::now());
        assert_eq!(panel.body_state(), BodyState::Searching);
    }

    #[gpui::test]
    fn an_invalid_regex_says_which_character_rather_than_claiming_no_results(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut panel = cx.update(panel);
        panel.query.set("(unclosed", Instant::now());
        panel.options.regex = true;
        panel.error = Some("regex parse error: unclosed group".to_string());
        assert_eq!(
            panel.body_state(),
            BodyState::InvalidQuery("regex parse error: unclosed group".to_string())
        );
        assert_eq!(panel.count_label(), "invalid pattern");
        assert_eq!(
            panel.body_message("main").as_deref(),
            Some("regex parse error: unclosed group")
        );
        assert!(!panel.body_state().has_results());
    }

    #[gpui::test]
    fn a_capped_search_says_so_in_the_count_row_not_only_in_a_tooltip(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut panel = cx.update(panel);
        panel.query.set("a", Instant::now());
        let mut capped = outcome(vec![("src/a.rs", 3)]);
        capped.truncated = true;
        complete(&mut panel, capped);
        assert_eq!(panel.count_label(), "3 results in 1 file (capped)");
        assert!(panel
            .truncation_tooltip()
            .expect("a tooltip")
            .contains("Stopped at 3 matches"));
    }

    #[gpui::test]
    fn fold_all_collapses_every_file_and_then_expands_every_file(cx: &mut gpui::TestAppContext) {
        let mut panel = cx.update(panel);
        panel.query.set("a", Instant::now());
        complete(&mut panel, outcome(vec![("src/a.rs", 1), ("src/b.rs", 1)]));

        assert!(!panel.all_collapsed(), "a fresh search opens everything");
        panel.toggle_fold_all();
        assert!(panel.all_collapsed());
        assert_eq!(panel.collapsed.len(), 2);
        panel.toggle_fold_all();
        assert!(!panel.all_collapsed());
        assert!(panel.collapsed.is_empty());
    }

    #[gpui::test]
    fn one_collapsed_file_out_of_two_still_offers_collapse_all(cx: &mut gpui::TestAppContext) {
        let mut panel = cx.update(panel);
        panel.query.set("a", Instant::now());
        complete(&mut panel, outcome(vec![("src/a.rs", 1), ("src/b.rs", 1)]));
        panel.collapsed.insert(PathBuf::from("/wt/src/a.rs"));
        assert!(
            !panel.all_collapsed(),
            "some open means the caret still offers to close them"
        );
        panel.toggle_fold_all();
        assert!(panel.all_collapsed());
    }

    #[gpui::test]
    fn tab_walks_only_the_fields_that_are_really_on_screen(cx: &mut gpui::TestAppContext) {
        let mut panel = cx.update(panel);
        assert_eq!(
            panel.next_visible_field(SearchField::Query),
            Some(SearchField::Query),
            "with replace and the globs hidden, the query is the only field there is"
        );

        panel.replace_open = true;
        assert_eq!(
            panel.next_visible_field(SearchField::Query),
            Some(SearchField::Replace)
        );
        assert_eq!(
            panel.next_visible_field(SearchField::Replace),
            Some(SearchField::Query)
        );

        panel.globs_open = true;
        assert_eq!(
            panel.next_visible_field(SearchField::Replace),
            Some(SearchField::Include)
        );
        assert_eq!(
            panel.next_visible_field(SearchField::Exclude),
            Some(SearchField::Query)
        );
    }

    #[gpui::test]
    fn each_field_has_its_own_text_and_its_own_focus_handle(cx: &mut gpui::TestAppContext) {
        let mut panel = cx.update(panel);
        let now = Instant::now();
        for (index, field) in SearchField::ALL.into_iter().enumerate() {
            panel.field_mut(field).set(&format!("value{index}"), now);
        }
        for (index, field) in SearchField::ALL.into_iter().enumerate() {
            assert_eq!(panel.field(field).as_str(), format!("value{index}"));
        }
        // Four distinct handles, not one shared one - see the field's own docs.
        let handles: Vec<gpui::FocusHandle> = SearchField::ALL
            .into_iter()
            .map(|field| panel.focus_handle(field).clone())
            .collect();
        for (index, handle) in handles.iter().enumerate() {
            for other in handles.iter().skip(index + 1) {
                assert_ne!(handle, other);
            }
        }
    }

    #[gpui::test]
    fn a_whitespace_query_is_a_real_search_not_the_idle_state(cx: &mut gpui::TestAppContext) {
        let mut panel = cx.update(panel);
        panel.query.set("    ", Instant::now());
        assert!(panel.has_query());
        assert_eq!(
            panel.body_state(),
            BodyState::Searching,
            "searching for an indentation width is a real thing to want"
        );
    }
}
