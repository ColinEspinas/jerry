//! In-file find (`mod+F`) - the find bar inside the focused file view, and its pure model.

use std::ops::Range;

use crate::root::plural;
use crate::search::engine::Matcher;

/// One hit in the open buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindHit {
    /// 1-based, matching `crate::root::AdeApp::code_cursor`'s own convention.
    pub line_number: usize,
    /// The byte range within that line.
    pub range: Range<usize>,
}

/// Every hit in `content`, in document order.
pub fn find_all(content: &str, matcher: &Matcher) -> Vec<FindHit> {
    content
        .lines()
        .enumerate()
        .flat_map(|(index, line)| {
            matcher
                .find_in_line(line)
                .into_iter()
                .map(move |range| FindHit {
                    line_number: index + 1,
                    range,
                })
        })
        .collect()
}

/// The find bar's whole state.
pub struct FindBar {
    pub query: crate::text_history::TextField,
    pub options: crate::search::engine::SearchOptions,
    /// Every hit against the buffer as it was when the query, the options or the content last
    /// changed.
    pub hits: Vec<FindHit>,
    /// Which hit is current, as an index into [`Self::hits`]. Meaningless - and never read - when
    /// `hits` is empty.
    pub current: usize,
    /// The query could not be compiled, with `regex`'s own message.
    pub error: Option<String>,
    /// A clone of `crate::root::AdeApp::find_bar_focus_handle` - the app's own permanent handle,
    /// not one minted per opening.
    pub focus_handle: gpui::FocusHandle,
}

impl FindBar {
    pub fn new(focus_handle: gpui::FocusHandle) -> Self {
        FindBar {
            query: crate::text_history::TextField::new(),
            options: crate::search::engine::SearchOptions::default(),
            hits: Vec::new(),
            current: 0,
            error: None,
            focus_handle,
        }
    }

    /// A genuinely empty field is the not-searched-yet state. Whitespace is a real query, exactly
    /// as it is in the panel (see `Matcher::compile`'s own docs).
    pub fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    /// Recomputes [`Self::hits`] against `content`, keeping the caret on the nearest hit at or
    /// after where it already was rather than snapping back to the top.
    pub fn recompute(&mut self, content: &str) {
        let previous_line = self.current_hit().map(|hit| hit.line_number);
        match Matcher::compile(self.query.as_str(), self.options) {
            Ok(Some(matcher)) => {
                self.error = None;
                self.hits = find_all(content, &matcher);
            }
            Ok(None) => {
                self.error = None;
                self.hits.clear();
            }
            Err(error) => {
                self.error = Some(error.0);
                self.hits.clear();
            }
        }
        self.current = match previous_line {
            Some(line) => self
                .hits
                .iter()
                .position(|hit| hit.line_number >= line)
                .unwrap_or(0),
            None => 0,
        };
    }

    /// The hit the viewport should be showing, if there is one.
    pub fn current_hit(&self) -> Option<&FindHit> {
        self.hits.get(self.current)
    }

    /// Steps to the next hit, wrapping - which is what every editor's find does, and what makes
    /// the count row's `N of M` honest about there being a cycle rather than an end.
    pub fn step_next(&mut self) -> Option<&FindHit> {
        if self.hits.is_empty() {
            return None;
        }
        self.current = (self.current + 1) % self.hits.len();
        self.current_hit()
    }

    /// The mirror of [`Self::step_next`].
    pub fn step_previous(&mut self) -> Option<&FindHit> {
        if self.hits.is_empty() {
            return None;
        }
        self.current = if self.current == 0 {
            self.hits.len() - 1
        } else {
            self.current - 1
        };
        self.current_hit()
    }

    /// The bar's own count readout, following the panel's three-state gate exactly:
    /// `""` while nothing is typed, `no results` for a real search that found nothing, and
    /// `3 of 12` once there are hits.
    pub fn count_label(&self) -> String {
        if !self.has_query() {
            return String::new();
        }
        if self.error.is_some() {
            return "invalid pattern".to_string();
        }
        if self.hits.is_empty() {
            return "no results".to_string();
        }
        format!("{} of {}", self.current + 1, self.hits.len())
    }

    /// Next/prev exist only when there is something to step through - the same gate the
    /// panel's fold-all and `Replace all` are behind.
    pub fn has_results(&self) -> bool {
        self.has_query() && self.error.is_none() && !self.hits.is_empty()
    }

    /// What the bar says when it has something to say beyond the count - the compile error, or
    /// nothing at all. Deliberately *not* a "not searched yet" sentence: unlike the panel, this
    /// bar sits directly above the file it searches, so the file itself is the empty state.
    pub fn notice(&self) -> Option<String> {
        self.error.clone()
    }

    /// The tooltip on next/prev, naming what stepping would land on.
    pub fn step_tooltip(&self, forward: bool) -> String {
        let direction = if forward { "Next" } else { "Previous" };
        format!(
            "{direction} of {}",
            plural::count(self.hits.len(), "match", Some("matches"))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::engine::SearchOptions;
    use std::time::Instant;

    const SAMPLE: &str = "let refresh_token = issue();\n\
                          if refresh_token.expired() { drop(refresh_token); }\n\
                          // nothing here\n\
                          fn refresh_token() {}\n";

    fn bar(cx: &mut gpui::App) -> FindBar {
        FindBar::new(cx.focus_handle())
    }

    fn typed(cx: &mut gpui::App, query: &str, options: SearchOptions) -> FindBar {
        let mut bar = bar(cx);
        bar.options = options;
        bar.query.set(query, Instant::now());
        bar.recompute(SAMPLE);
        bar
    }

    #[gpui::test]
    fn an_empty_field_is_not_searched_yet_and_offers_nothing_to_step_through(
        cx: &mut gpui::TestAppContext,
    ) {
        let bar = cx.update(bar);
        assert_eq!(bar.count_label(), "");
        assert!(!bar.has_results());
        assert_eq!(bar.notice(), None);
    }

    #[gpui::test]
    fn a_real_query_finds_every_hit_including_two_on_one_line(cx: &mut gpui::TestAppContext) {
        let bar = cx.update(|cx| typed(cx, "refresh_token", SearchOptions::default()));
        assert_eq!(
            bar.hits
                .iter()
                .map(|hit| hit.line_number)
                .collect::<Vec<_>>(),
            vec![1, 2, 2, 4],
            "line 2 holds two hits, and they are two results - the same rule the panel's tree \
             follows"
        );
        assert_eq!(bar.count_label(), "1 of 4");
    }

    #[gpui::test]
    fn a_real_query_with_no_hits_says_no_results_rather_than_nothing(
        cx: &mut gpui::TestAppContext,
    ) {
        let bar = cx.update(|cx| typed(cx, "nonexistent", SearchOptions::default()));
        assert_eq!(bar.count_label(), "no results");
        assert!(!bar.has_results());
        assert_eq!(
            bar.current_hit(),
            None,
            "nothing to jump to, so the viewport must not move"
        );
    }

    #[gpui::test]
    fn next_and_previous_walk_every_hit_and_wrap(cx: &mut gpui::TestAppContext) {
        let mut bar = cx.update(|cx| typed(cx, "refresh_token", SearchOptions::default()));
        let total = bar.hits.len();
        assert_eq!(bar.count_label(), "1 of 4");
        for step in 1..total {
            bar.step_next();
            assert_eq!(bar.count_label(), format!("{} of {total}", step + 1));
        }
        bar.step_next();
        assert_eq!(
            bar.count_label(),
            "1 of 4",
            "wrapping is what every editor's find does, and the count says there is a cycle"
        );
        bar.step_previous();
        assert_eq!(bar.count_label(), "4 of 4");
    }

    #[gpui::test]
    fn stepping_an_empty_result_set_is_a_no_op_rather_than_a_panic(cx: &mut gpui::TestAppContext) {
        let mut bar = cx.update(|cx| typed(cx, "nonexistent", SearchOptions::default()));
        assert_eq!(bar.step_next(), None);
        assert_eq!(bar.step_previous(), None);
    }

    #[gpui::test]
    fn the_modifier_buttons_mean_here_exactly_what_they_mean_in_the_panel(
        cx: &mut gpui::TestAppContext,
    ) {
        let sensitive = cx.update(|cx| {
            typed(
                cx,
                "REFRESH_TOKEN",
                SearchOptions {
                    match_case: true,
                    ..SearchOptions::default()
                },
            )
        });
        assert_eq!(sensitive.count_label(), "no results");

        let insensitive = cx.update(|cx| typed(cx, "REFRESH_TOKEN", SearchOptions::default()));
        assert_eq!(insensitive.hits.len(), 4);

        let whole_word = cx.update(|cx| {
            typed(
                cx,
                "token",
                SearchOptions {
                    whole_word: true,
                    ..SearchOptions::default()
                },
            )
        });
        assert_eq!(
            whole_word.count_label(),
            "no results",
            "`_` is a word character, so `refresh_token` does not contain the whole word `token`"
        );

        let regex = cx.update(|cx| {
            typed(
                cx,
                r"fn \w+\(",
                SearchOptions {
                    regex: true,
                    ..SearchOptions::default()
                },
            )
        });
        assert_eq!(regex.count_label(), "1 of 1");
    }

    #[gpui::test]
    fn an_invalid_regex_reports_itself_rather_than_claiming_the_file_is_empty(
        cx: &mut gpui::TestAppContext,
    ) {
        let bar = cx.update(|cx| {
            typed(
                cx,
                "(unclosed",
                SearchOptions {
                    regex: true,
                    ..SearchOptions::default()
                },
            )
        });
        assert_eq!(bar.count_label(), "invalid pattern");
        assert!(bar.notice().is_some());
        assert!(!bar.has_results());
    }

    #[gpui::test]
    fn narrowing_a_query_keeps_the_caret_near_where_it_already_was(cx: &mut gpui::TestAppContext) {
        let mut bar = cx.update(|cx| typed(cx, "refresh_token", SearchOptions::default()));
        bar.step_next();
        bar.step_next();
        bar.step_next();
        assert_eq!(bar.current_hit().expect("a hit").line_number, 4);

        // One more character typed: the hit set shrinks, and the bar must stay near line 4 rather
        // than dragging the viewport back to the top of the file.
        bar.query.insert_str("(", Instant::now());
        bar.recompute(SAMPLE);
        assert_eq!(bar.hits.len(), 1);
        assert_eq!(bar.current_hit().expect("a hit").line_number, 4);
    }

    #[gpui::test]
    fn the_step_tooltip_goes_through_the_pluralisation_helper(cx: &mut gpui::TestAppContext) {
        let bar = cx.update(|cx| typed(cx, "// nothing", SearchOptions::default()));
        assert_eq!(bar.step_tooltip(true), "Next of 1 match");
        let many = cx.update(|cx| typed(cx, "refresh_token", SearchOptions::default()));
        assert_eq!(many.step_tooltip(false), "Previous of 4 matches");
    }
}
