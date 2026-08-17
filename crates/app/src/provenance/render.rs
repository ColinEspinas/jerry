//! The visible half of per-agent attribution (GitHub issue #287): the diff view's per-line
//! gutter bar, the file row's author chips and their `⚠` ring, and the per-author filter the
//! ring opens.

use std::path::{Path, PathBuf};

use gpui::{div, font, prelude::*, px, ClickEvent, Context, Rgba};

use super::change_set::ChangeSetEntry;
use super::{AgentKey, Author};
use crate::root::widgets::text_tooltip;
use crate::root::AdeApp;
use crate::theme;
use crate::work_surface;
use crate::work_surface::agents::ProcessKind;

/// How far a line that is **not** the filtered author's is dimmed while a per-author filter is
/// active - `0.32` opacity, an acceptance criterion of GitHub issue #287.
pub const FILTER_DIM_OPACITY: f32 = 0.32;

/// One author, as this app draws it: the tint its gutter bar and chip wear, the single glyph the
/// chip shows, and the sentence a tooltip states.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthorStyle {
    /// The gutter bar's colour, and the chip's glyph colour - one value, so a line's bar and its
    /// author's chip are visibly the same author.
    pub fg: Rgba,
    /// The chip's fill.
    pub bg: Rgba,
    /// The chip's single character.
    pub initial: &'static str,
    /// What this author is called in prose - a tooltip, or the filter indicator's `<label> only`.
    pub label: String,
}

/// How `author` is drawn, or `None` when it must not be drawn at all.
pub fn author_style(author: &Author) -> Option<AuthorStyle> {
    match author {
        Author::Agent(key) => agent_style(key),
        // Orca's second rule, and `STAGE-A-CHANGELOG.md` §1's own wording for the tooltip,
        // verbatim: *`'you'` renders neutral `#4e545a` and is labelled you — hand edit*.
        Author::You => Some(AuthorStyle {
            fg: theme::changes::HAND_EDIT_CHIP_FG.into(),
            bg: theme::changes::HAND_EDIT_CHIP_BG.into(),
            initial: "\u{b7}",
            label: "you".to_string(),
        }),
        Author::Unattributed => None,
    }
}

fn agent_style(key: &AgentKey) -> Option<AuthorStyle> {
    let kind = ProcessKind::Agent(key.kind()?);
    let (fg, bg) = work_surface::state::agent_tint(kind);
    Some(AuthorStyle {
        fg,
        bg,
        initial: work_surface::state::agent_initial(kind),
        label: kind.label().to_string(),
    })
}

/// This agent's `(fg, bg)` from [`crate::theme::agent`]'s pool - the colour half of
/// [`agent_style`], without the owned label, for the paths that run per diff line per frame.
fn agent_tint_of(key: &AgentKey) -> Option<(Rgba, Rgba)> {
    Some(work_surface::state::agent_tint(ProcessKind::Agent(
        key.kind()?,
    )))
}

/// The colour of `author`'s gutter bar, or `None` for a line that carries no bar at all.
pub fn author_gutter_color(author: &Author) -> Option<Rgba> {
    match author {
        Author::You => Some(theme::changes::HAND_EDIT_GUTTER.into()),
        Author::Agent(key) => agent_tint_of(key).map(|(fg, _)| fg),
        Author::Unattributed => None,
    }
}

/// The sentence a gutter bar's, or a chip's, tooltip states - `None` for an author that is not
/// drawn at all, which therefore has no tooltip either.
pub fn author_tooltip(author: &Author) -> Option<String> {
    match author {
        Author::You => Some("you \u{2014} hand edit".to_string()),
        other => author_style(other).map(|style| format!("{} \u{2014} wrote this", style.label)),
    }
}

/// Whether one diff line is dimmed to [`FILTER_DIM_OPACITY`] by an active per-author filter.
pub fn line_is_dimmed(author: Option<&Author>, filter: Option<&Author>) -> bool {
    match (author, filter) {
        // `is_drawable(author)` rather than `author.is_some()`: a context or unattributed line
        // arrives as `Some(Author::Unattributed)`, which is the absence of an answer, and an
        // absent answer is nobody's line to dim.
        (Some(author), Some(filter)) => is_drawable(author) && author != filter,
        _ => false,
    }
}

/// The `⚠` ring's tooltip, verbatim from `STAGE-A-CHANGELOG.md` §1's own `title=` attribute:
pub const SHARED_RING_TOOLTIP: &str =
    "Two agents edited this file \u{2014} click to filter the diff by author";

/// The filter indicator's tooltip, verbatim from the design.
pub const FILTER_INDICATOR_TOOLTIP: &str =
    "Showing one author's lines \u{2014} click to show every author";

/// The `crate::keymap::resolve_combo` spec for the chip's own filter gesture, and the one the
/// Changes footer renders as keycaps (`STAGE-A-CHANGELOG.md` §2's `⌥click filter by author`).
pub const AUTHOR_FILTER_SPEC: &str = "alt+click";

/// Which path a per-author filter is pinned to, and to whom.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthorFilter {
    pub path: PathBuf,
    pub author: Author,
}

impl AdeApp {
    /// The per-author filter that is genuinely in force for the diff on screen right now, if any.
    pub(crate) fn active_author_filter(&self) -> Option<&Author> {
        let filter = self.author_filter.as_ref()?;
        (self.open_change.as_deref() == Some(filter.path.as_path())).then_some(&filter.author)
    }

    /// Opens `path`'s diff filtered to `author` - the `⚠` ring's whole behaviour, and
    /// `alt+click`'s.
    pub(crate) fn filter_diff_by_author(
        &mut self,
        path: PathBuf,
        author: Author,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.open_change_diff(path.clone(), window, cx);
        self.author_filter = Some(AuthorFilter { path, author });
        cx.notify();
    }

    /// Clears the filter - the `✕` on the indicator.
    pub(crate) fn clear_author_filter(&mut self, cx: &mut Context<Self>) {
        if self.author_filter.take().is_some() {
            cx.notify();
        }
    }

    /// Whether the current worktree really has more than one agent open in it - rule 3 of this
    /// module's docs, and the gate on every author chip in the app.
    pub(crate) fn worktree_has_multiple_agents(&self) -> bool {
        self.agents
            .iter_for_cwd(self.diff_root.clone())
            .filter(|agent| agent.kind.is_agent_session())
            .count()
            > 1
    }

    /// Every diff line's author for the file on screen, hunk by hunk and index-aligned with
    /// `file.hunks[h].lines`, or an empty `Vec` when this worktree has no provenance record for
    /// this path at all.
    pub(crate) fn diff_line_authors(&self, file: &wt_core::diff::DiffFile) -> Vec<Vec<Author>> {
        let Some(records) = self.line_provenance.worktree(&self.diff_root) else {
            return Vec::new();
        };
        if records.get(&file.path).is_none() {
            return Vec::new();
        }
        super::change_set::line_authors(file, Some(records))
    }

    /// One file row's author chips, with the `⚠` ring around them when the file has more than one
    /// agent in it - `REVISION-2026-08-14.md` §1 and `REVISION-2026-07-31.md` §4.
    pub(crate) fn render_author_chips(
        &self,
        id: &'static str,
        path: &Path,
        authors: &[Author],
        shared: bool,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.worktree_has_multiple_agents() {
            return None;
        }
        let drawable: Vec<(Author, AuthorStyle)> = authors
            .iter()
            .filter_map(|author| Some((author.clone(), author_style(author)?)))
            .collect();
        let first = drawable.first()?.0.clone();

        let element_id = format!("{id}-authors-{}", path.display());
        // The selector states which of the two states the group is really in, the same way the
        // change row's own `-name-seen`/`-name-unseen` already does - so "the ring is lit" is a
        // fact a render test can assert against real painted output rather than infer.
        let group_selector = if shared {
            format!("{id}-authors-ring-{}", path.display())
        } else {
            element_id.clone()
        };
        let ring_path = path.to_path_buf();
        let mut group = div()
            .id(gpui::SharedString::from(element_id))
            .debug_selector(move || group_selector)
            .flex_none()
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(1.0))
            .rounded(theme::radius::BUTTON)
            .border_1()
            // Always a real 1px border, only ever recoloured - the same "always paint the box"
            // convention the change row's own selection edge uses, so lighting the ring cannot
            // shift the chips beside it by a pixel.
            .border_color(if shared {
                theme::changes::SHARED_RING.into()
            } else {
                work_surface::state::TRANSPARENT
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.filter_diff_by_author(ring_path.clone(), first.clone(), window, cx);
            }));
        if shared {
            group = group.tooltip(text_tooltip(SHARED_RING_TOOLTIP));
        }

        for (author, style) in drawable {
            let chip_path = path.to_path_buf();
            let chip_author = author.clone();
            // Deliberately *not* derived from the group's own selector: that one changes when the
            // ring lights, and a chip's identity must not move with a fact about its neighbours.
            let chip_selector = format!(
                "{id}-author-{}-{}",
                path.display(),
                author_selector_key(&author)
            );
            group = group.child(
                div()
                    .id(gpui::SharedString::from(chip_selector.clone()))
                    .debug_selector(move || chip_selector)
                    .flex_none()
                    .cursor_pointer()
                    .when_some(author_tooltip(&author), |el, tip| {
                        el.tooltip(text_tooltip(tip))
                    })
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        // Only the *modified* click belongs to the chip. A plain click on a
                        // chip is a click on the ring - same gesture, same destination - so
                        // it is deliberately left to bubble to the group above.
                        if !event.modifiers().alt {
                            return;
                        }
                        cx.stop_propagation();
                        this.filter_diff_by_author(
                            chip_path.clone(),
                            chip_author.clone(),
                            window,
                            cx,
                        );
                    }))
                    .child(self.render_author_chip(&author, &style)),
            );
        }
        Some(group.into_any_element())
    }

    /// A read-only strip of author chips - the same marks [`Self::render_author_chips`] draws,
    /// with no ring and no click behind them.
    pub(crate) fn render_author_chip_strip(
        &self,
        id: &'static str,
        authors: &[Author],
    ) -> Option<gpui::AnyElement> {
        if !self.worktree_has_multiple_agents() {
            return None;
        }
        let drawable: Vec<(Author, AuthorStyle)> = authors
            .iter()
            .filter_map(|author| Some((author.clone(), author_style(author)?)))
            .collect();
        if drawable.is_empty() {
            return None;
        }
        let mut strip = div()
            .debug_selector(move || format!("{id}-authors"))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(2.0));
        for (author, style) in drawable {
            let chip_selector = format!("{id}-authors-{}", author_selector_key(&author));
            strip = strip.child(
                div()
                    .id(gpui::SharedString::from(chip_selector.clone()))
                    .debug_selector(move || chip_selector)
                    .flex_none()
                    .when_some(author_tooltip(&author), |el, tip| {
                        el.tooltip(text_tooltip(tip))
                    })
                    .child(self.render_author_chip(&author, &style)),
            );
        }
        Some(strip.into_any_element())
    }

    /// One author chip, at the mock's own 13px box.
    pub(crate) fn render_author_chip(
        &self,
        author: &Author,
        style: &AuthorStyle,
    ) -> gpui::AnyElement {
        if let Author::Agent(key) = author {
            if let Some(kind) = key.kind() {
                return self.render_agent_chip_icon(ProcessKind::Agent(kind), px(13.0), px(7.5));
            }
        }
        div()
            .flex_none()
            .w(px(13.0))
            .h(px(13.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::radius::CHIP)
            .bg(style.bg)
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(7.5))
            .text_color(style.fg)
            .child(style.initial)
            .into_any_element()
    }

    /// The `<agent> only ✕` indicator, in the file toolbar, **only while a filter is active** -
    /// `STAGE-A-CHANGELOG.md` §4b:
    pub(crate) fn render_author_filter_indicator(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let author = self.active_author_filter()?;
        let style = author_style(author)?;
        let dot = author_gutter_color(author)?;
        // `your lines only` rather than `you only` - the design's own wording.
        let label = match author {
            Author::You => "your lines".to_string(),
            _ => style.label.clone(),
        };
        Some(
            div()
                .id("diff-author-filter")
                .debug_selector(|| "diff-author-filter".to_string())
                .flex_none()
                .flex()
                .items_center()
                .gap(px(5.0))
                .h(px(16.0))
                .px(px(6.0))
                .rounded(theme::radius::CHIP)
                .bg(theme::changes::FILTER_BG)
                .hover(|el| el.bg(theme::changes::FILTER_HOVER_BG))
                .cursor_pointer()
                .tooltip(text_tooltip(FILTER_INDICATOR_TOOLTIP))
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.clear_author_filter(cx);
                }))
                .child(
                    div()
                        .flex_none()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded(px(1.0))
                        .bg(dot),
                )
                .child(
                    div()
                        .flex_none()
                        .font(font(theme::font::MONO))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(px(9.5))
                        .text_color(theme::text::BODY)
                        .child(format!("{label} only")),
                )
                .child(
                    div()
                        .flex_none()
                        .debug_selector(|| "diff-author-filter-clear".to_string())
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
                        .text_color(theme::text::DIMMER)
                        .child("\u{2715}"),
                )
                .into_any_element(),
        )
    }
}

/// The authors a row draws chips for: everyone the change set really recorded, minus the ones
/// this app must not draw (see [`author_style`]).
pub fn chip_authors(entry: &ChangeSetEntry) -> Vec<Author> {
    drawable(entry.authors())
}

/// The same, for a whole worktree - the graph's working-tree row `by` union.
pub fn chip_authors_for(change_set: &super::change_set::ChangeSet) -> Vec<Author> {
    drawable(change_set.authors())
}

fn drawable(authors: Vec<Author>) -> Vec<Author> {
    authors.into_iter().filter(is_drawable).collect()
}

/// Whether `author` is one this app can draw at all - [`author_style`]'s own `Some`/`None`
/// question, answered without building the [`AuthorStyle`] (and its owned label) that a caller
/// asking only "is there anything here?" would immediately throw away.
pub fn is_drawable(author: &Author) -> bool {
    match author {
        Author::Agent(key) => key.kind().is_some(),
        Author::You => true,
        Author::Unattributed => false,
    }
}

/// Whether this row has anything to draw a chip for - the cheap form of `!chip_authors(..)
/// .is_empty()`, for the per-frame gate that only needs the yes/no
/// (`crate::sidebar::render::AdeApp::change_author_filter_live` walks every row of the change
/// set on every frame).
pub fn has_drawable_author(entry: &ChangeSetEntry) -> bool {
    entry
        .split()
        .iter()
        .any(|(author, stat)| !stat.is_empty() && is_drawable(author))
}

/// A stable, filesystem-safe fragment naming one author inside a `debug_selector` - the durable
/// agent key is a worktree path plus a spawn second, which would make a selector that no test
/// could name. The kind label is what a test actually wants to point at ("the Claude chip").
fn author_selector_key(author: &Author) -> String {
    match author {
        Author::Agent(key) => key
            .kind()
            .map(|kind| kind.label().to_lowercase())
            .unwrap_or_else(|| "agent".to_string()),
        Author::You => "you".to_string(),
        Author::Unattributed => "unattributed".to_string(),
    }
}

/// The pure half of attribution rendering: one author to one tint, glyph and sentence, and the
/// dim predicate a filter runs on every line.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_surface::agents::AgentKind;

    fn agent(kind: AgentKind) -> Author {
        Author::Agent(AgentKey::new(crate::review::state::baseline_key(
            Path::new("/repo/wt-a"),
            kind,
            1_700_000_000,
        )))
    }

    #[test]
    fn two_agents_and_a_hand_edit_are_three_distinct_gutter_tints() {
        let claude = author_gutter_color(&agent(AgentKind::Claude)).expect("claude has a tint");
        let codex = author_gutter_color(&agent(AgentKind::Codex)).expect("codex has a tint");
        let hand = author_gutter_color(&Author::You).expect("`you` has the neutral token");

        assert_ne!(claude, codex, "two agents must never share a gutter tint");
        assert_ne!(claude, hand);
        assert_ne!(codex, hand);
        assert_eq!(
            hand,
            theme::changes::HAND_EDIT_GUTTER.into(),
            "`you` renders the neutral hand-edit token, not an agent tint - Orca's second rule"
        );
    }

    #[test]
    fn an_unattributed_line_gets_no_bar_no_chip_and_no_tooltip() {
        assert_eq!(author_style(&Author::Unattributed), None);
        assert_eq!(author_gutter_color(&Author::Unattributed), None);
        assert_eq!(author_tooltip(&Author::Unattributed), None);
    }

    #[test]
    fn an_agent_key_this_build_cannot_read_is_drawn_as_nothing_rather_than_as_a_guess() {
        let future = Author::Agent(AgentKey::new("utf8:/repo/wt-a|Opus|1700000000"));
        assert_eq!(future.agent().and_then(AgentKey::kind), None);
        assert_eq!(author_style(&future), None);
        assert_eq!(author_gutter_color(&future), None);
        assert!(
            chip_authors_for(&super::super::change_set::ChangeSet::default()).is_empty(),
            "and it is filtered out of a chip group rather than drawn blank"
        );
    }

    #[test]
    fn an_agent_key_whose_worktree_path_contains_a_pipe_still_resolves_its_kind() {
        let key = AgentKey::new(crate::review::state::baseline_key(
            Path::new("/repo/odd|name"),
            AgentKind::Codex,
            1_700_000_000,
        ));
        assert_eq!(key.kind(), Some(AgentKind::Codex));
    }

    #[test]
    fn the_hand_edit_tooltip_is_the_designs_own_wording() {
        assert_eq!(
            author_tooltip(&Author::You).as_deref(),
            Some("you \u{2014} hand edit")
        );
    }

    #[test]
    fn a_filter_dims_other_authors_lines_and_nothing_else() {
        let claude = agent(AgentKind::Claude);
        let codex = agent(AgentKind::Codex);

        assert!(
            line_is_dimmed(Some(&codex), Some(&claude)),
            "another author's line is the one thing a filter dims"
        );
        assert!(
            !line_is_dimmed(Some(&claude), Some(&claude)),
            "the filtered author's own lines stay at full opacity - they are the point"
        );
        assert!(
            !line_is_dimmed(Some(&Author::Unattributed), Some(&claude)),
            "a line nobody wrote is not somebody else's line"
        );
        assert!(
            !line_is_dimmed(None, Some(&claude)),
            "and neither is a line with no author at all"
        );
        assert!(
            !line_is_dimmed(Some(&codex), None),
            "with no filter in force nothing dims"
        );
        assert_eq!(
            FILTER_DIM_OPACITY, 0.32,
            "the mock's own opacity, quoted as an acceptance criterion"
        );
    }

    #[test]
    fn the_cheap_drawable_check_agrees_with_the_full_style_for_every_author() {
        for author in [
            agent(AgentKind::Claude),
            agent(AgentKind::Codex),
            Author::Agent(AgentKey::new("utf8:/repo/wt-a|Opus|1700000000")),
            Author::Agent(AgentKey::new("not-a-key")),
            Author::You,
            Author::Unattributed,
        ] {
            assert_eq!(
                is_drawable(&author),
                author_style(&author).is_some(),
                "{author:?}"
            );
        }
    }

    #[test]
    fn the_shared_file_ring_is_the_apps_own_attention_amber() {
        assert_eq!(
            theme::changes::SHARED_RING.resolve(),
            theme::status::ASK_CARD_EDGE.resolve(),
            "amber-means-attention is one language; a shared file genuinely needs attention"
        );
    }
}

/// The mock's shared-file sad path, rendered for real (GitHub issue #287's acceptance criteria).
#[cfg(test)]
mod attribution_render_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests::open_test_app;
    use crate::sidebar::render::RightSidebarView;
    use crate::work_surface::agents::AgentKind;
    use gpui::TestAppContext;
    use tempfile::TempDir;
    use test_support::git;

    /// The mock's own `src/api/users.rs`, as committed.
    const BASE: &str = "\
impl UserApi {
    pub async fn list(&self, page: Page) -> Result<Vec<User>> {
        let sql = self.orm.select(&[\"id\", \"email\"]);
    }

    pub async fn search(&self, term: &str) -> Result<Vec<User>> {
        let rows = self.pool.query(SEARCH_SQL, &[&term]).await?;
        Ok(rows.into_iter().map(User::from).collect())
    }
}
";

    /// After the first agent rewrote the `list` body.
    const AFTER_FIRST: &str = "\
impl UserApi {
    pub async fn list(&self, page: Page) -> Result<Vec<User>> {
        let q = QueryBuilder::table(\"users\").select(&[\"id\", \"email\"]);
    }

    pub async fn search(&self, term: &str) -> Result<Vec<User>> {
        let rows = self.pool.query(SEARCH_SQL, &[&term]).await?;
        Ok(rows.into_iter().map(User::from).collect())
    }
}
";

    /// After the second agent rewrote `search`, in the same checkout.
    const AFTER_SECOND: &str = "\
impl UserApi {
    pub async fn list(&self, page: Page) -> Result<Vec<User>> {
        let q = QueryBuilder::table(\"users\").select(&[\"id\", \"email\"]);
    }

    pub async fn search(&self, term: &str) -> Result<Vec<User>> {
        let rows = self.cache.get_or_load(term, || {
            self.pool.query(SEARCH_SQL, &[&term])
        }).await?;
        Ok(rows.into_iter().map(User::from).collect())
    }
}
";

    /// And after the human's own one-line hand edit - the mock's `'you'` line, verbatim.
    const AFTER_HAND_EDIT: &str = "\
impl UserApi {
    pub async fn list(&self, page: Page) -> Result<Vec<User>> {
        let q = QueryBuilder::table(\"users\").select(&[\"id\", \"email\"]);
    }

    pub async fn search(&self, term: &str) -> Result<Vec<User>> {
        let rows = self.cache.get_or_load(term, || {
            self.pool.query(SEARCH_SQL, &[&term])
        }).await?;
        Ok(rows.into_iter().map(User::from).collect())
        // TODO: cache key must include tenant_id
    }
}
";

    const SHARED_PATH: &str = "src/api/users.rs";
    const RING: &str = "change-row-authors-ring-src/api/users.rs";
    const CHIP_CLAUDE: &str = "change-row-author-src/api/users.rs-claude";
    const CHIP_CODEX: &str = "change-row-author-src/api/users.rs-codex";
    const CHIP_YOU: &str = "change-row-author-src/api/users.rs-you";

    /// A real repo whose one shared file carries lines from two different agents *and* one hand
    /// edit, with the provenance really recorded for all three.
    fn shared_file_repo() -> (TempDir, super::super::store::ProvenanceStore) {
        let dir = TempDir::new().expect("tempdir");
        // Canonicalized because `AdeApp` canonicalizes the root it is given
        // (`crate::rail::repo::canonical_repo_path`) and then keys provenance by exact `PathBuf`.
        // On macOS `std::env::temp_dir()` is itself behind a `/var` -> `/private/var` symlink, so
        // recording against `TempDir::path()` verbatim writes keys the app can never look up.
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        let repo = root.as_path();
        git(repo, &["init", "-b", "main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test User"]);
        std::fs::create_dir_all(repo.join("src/api")).expect("mkdir");
        std::fs::write(repo.join(SHARED_PATH), BASE).expect("seed");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-m", "initial"]);

        let file = repo.join(SHARED_PATH);
        let mut store = super::super::store::ProvenanceStore::default();
        // Two *different* agent kinds, which is what makes the mock's "three distinct tints"
        // reachable at all: this app allocates a tint per agent CLI
        // (`crate::work_surface::state::agent_tint`), so `sonnet-4.5` and `haiku-4.5` are Claude
        // and Codex here.
        for (kind, spawned_at, after) in [
            (AgentKind::Claude, 1_700_000_000, AFTER_FIRST),
            (AgentKind::Codex, 1_700_000_900, AFTER_SECOND),
        ] {
            let key = AgentKey::new(crate::review::state::baseline_key(repo, kind, spawned_at));
            store.begin_agent_edit(repo, &file);
            std::fs::write(&file, after).expect("the agent's own write");
            store.record_agent_edit(repo, &file, &key);
        }
        std::fs::write(&file, AFTER_HAND_EDIT).expect("hand edit");
        store.record_hand_edit(repo, &file);
        (dir, store)
    }

    /// Opens the app on `repo` with the Changes panel showing, `agents` real agent sessions in the
    /// worktree, and `store`'s provenance installed - i.e. the mock's own `Review · uncommitted`
    /// state, for real.
    fn open_changes<'a>(
        cx: &'a mut TestAppContext,
        repo: &TempDir,
        store: super::super::store::ProvenanceStore,
        agents: &[ProcessKind],
    ) -> (gpui::Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
            for kind in agents {
                app.new_agent(*kind, window, cx);
            }
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.line_provenance = store;
            app.rebuild_change_set();
            cx.notify();
        });
        cx.run_until_parked();
        (app, cx)
    }

    #[gpui::test]
    fn the_shared_file_row_carries_three_author_chips_inside_the_ring(cx: &mut TestAppContext) {
        let (repo, store) = shared_file_repo();
        let (app, cx) = open_changes(
            cx,
            &repo,
            store,
            &[ProcessKind::claude(), ProcessKind::codex()],
        );

        app.read_with(cx, |app, _| {
            let entry = app
                .uncommitted_change_set
                .entry(Path::new(SHARED_PATH))
                .expect("premise: the shared file is one row of the change set");
            assert_eq!(
                chip_authors(entry).len(),
                3,
                "two agents and the human's own hand edit - `you` is a first-class author"
            );
            assert!(entry.is_shared(), "premise: more than one agent wrote here");
        });

        let ring = cx
            .debug_bounds(RING)
            .expect("the ⚠ ring must really be painted around the chips");
        for chip in [CHIP_CLAUDE, CHIP_CODEX, CHIP_YOU] {
            let bounds = cx
                .debug_bounds(chip)
                .unwrap_or_else(|| panic!("{chip} must be painted"));
            assert!(
                ring.contains(&bounds.center()),
                "{chip} must sit inside the ring, not beside it"
            );
        }
    }

    #[gpui::test]
    fn a_file_only_one_agent_wrote_gets_chips_but_no_ring(cx: &mut TestAppContext) {
        let dir = TempDir::new().expect("tempdir");
        // Canonicalized because `AdeApp` canonicalizes the root it is given
        // (`crate::rail::repo::canonical_repo_path`) and then keys provenance by exact `PathBuf`.
        // On macOS `std::env::temp_dir()` is itself behind a `/var` -> `/private/var` symlink, so
        // recording against `TempDir::path()` verbatim writes keys the app can never look up.
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        let repo = root.as_path();
        git(repo, &["init", "-b", "main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test User"]);
        std::fs::write(repo.join("solo.rs"), "fn solo() {}\n").expect("seed");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-m", "initial"]);

        let file = repo.join("solo.rs");
        let mut store = super::super::store::ProvenanceStore::default();
        let key = AgentKey::new(crate::review::state::baseline_key(
            repo,
            AgentKind::Claude,
            1_700_000_000,
        ));
        store.begin_agent_edit(repo, &file);
        std::fs::write(&file, "fn solo() {}\nfn added() {}\n").expect("write");
        store.record_agent_edit(repo, &file, &key);

        let (_app, cx) = open_changes(
            cx,
            &dir,
            store,
            &[ProcessKind::claude(), ProcessKind::codex()],
        );

        assert!(
            cx.debug_bounds("change-row-author-solo.rs-claude")
                .is_some(),
            "the one agent that wrote it still gets its chip"
        );
        assert!(
            cx.debug_bounds("change-row-authors-ring-solo.rs").is_none(),
            "but no ring: one agent wrote this file"
        );
        assert!(
            cx.debug_bounds("change-row-authors-solo.rs").is_some(),
            "the chip group is there, un-ringed"
        );
    }

    #[gpui::test]
    fn a_single_agent_worktree_shows_no_chips_no_ring_and_no_footer_hint(cx: &mut TestAppContext) {
        let (repo, store) = shared_file_repo();
        let (app, cx) = open_changes(cx, &repo, store, &[ProcessKind::claude()]);

        app.read_with(cx, |app, _| {
            assert!(
                !app.worktree_has_multiple_agents(),
                "premise: one agent session in this worktree"
            );
            assert!(
                app.uncommitted_change_set
                    .entry(Path::new(SHARED_PATH))
                    .expect("row")
                    .is_shared(),
                "premise: the file itself is still genuinely shared - only the *display* is gated"
            );
        });

        assert!(cx.debug_bounds(CHIP_CLAUDE).is_none());
        assert!(cx.debug_bounds(CHIP_YOU).is_none());
        assert!(cx.debug_bounds(RING).is_none());
        assert!(
            cx.debug_bounds("changes-footer-author-filter-hint")
                .is_none(),
            "and the footer must not advertise ⌥click when there is no chip to click"
        );
    }

    #[gpui::test]
    fn the_footer_advertises_alt_click_exactly_when_there_is_a_chip_to_click(
        cx: &mut TestAppContext,
    ) {
        let (repo, store) = shared_file_repo();
        let (_app, cx) = open_changes(
            cx,
            &repo,
            store,
            &[ProcessKind::claude(), ProcessKind::codex()],
        );
        let hint = cx
            .debug_bounds("changes-footer-author-filter-hint")
            .expect("STAGE-A-CHANGELOG.md §2's `\u{2325}click filter by author`, as real keycaps");
        // And it really fits. This strip is a fixed-width band inside a ~315px panel, so a third
        // hint is exactly the kind of thing that ends up *past* the right edge and is never seen -
        // which is why the prose lead-in is now the only shrinkable thing in the row
        // (`REVISION-2026-08-14.md` §4's rule for the rail's repo header: same shape of problem,
        // same fix). Caught by a real screenshot of the running app, then pinned here.
        let footer = cx.debug_bounds("changes-footer").expect("the footer band");
        assert!(
            hint.origin.x + hint.size.width <= footer.origin.x + footer.size.width,
            "the hint must fit inside the band, not run off its right edge: hint ends at {:?}, \
             band ends at {:?}",
            hint.origin.x + hint.size.width,
            footer.origin.x + footer.size.width
        );
    }

    #[gpui::test]
    fn the_diff_gutter_paints_an_author_bar_beside_the_kind_accent(cx: &mut TestAppContext) {
        let (repo, store) = shared_file_repo();
        let (app, cx) = open_changes(
            cx,
            &repo,
            store,
            &[ProcessKind::claude(), ProcessKind::codex()],
        );

        let row = cx
            .debug_bounds(sel(format!("change-row-{SHARED_PATH}")))
            .expect("the shared row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        // How many of the file's rendered lines really have an author, and how many do not -
        // both read off the same function the gutter itself reads.
        let (attributed, unattributed) = app.read_with(cx, |app, _| {
            let file = app
                .open_diff_file_cache
                .as_ref()
                .expect("premise: the click opened the file's diff")
                .clone();
            let authors = app.diff_line_authors(&file);
            let flat: Vec<Author> = authors.into_iter().flatten().collect();
            let attributed = flat
                .iter()
                .filter(|author| author_style(author).is_some())
                .count();
            (attributed, flat.len() - attributed)
        });
        assert!(
            attributed >= 3,
            "premise: this file has lines from three different authors, not {attributed}"
        );

        // The issue's own acceptance criterion, stated as the thing you can see: *"the open diff
        // shows three distinct gutter tints, one of which is the neutral hand-edit tint"*.
        let tints: std::collections::BTreeSet<String> = app.read_with(cx, |app, _| {
            let file = app
                .open_diff_file_cache
                .as_ref()
                .expect("open file")
                .clone();
            app.diff_line_authors(&file)
                .into_iter()
                .flatten()
                .filter_map(|author| author_gutter_color(&author))
                .map(|rgba| format!("{rgba:?}"))
                .collect()
        });
        assert_eq!(
            tints.len(),
            3,
            "three authors must read as three colours, not as `an agent wrote this`: {tints:?}"
        );
        assert!(
            tints.contains(&format!(
                "{:?}",
                Rgba::from(theme::changes::HAND_EDIT_GUTTER)
            )),
            "and one of them is the neutral hand-edit tint - Orca's second rule, on screen"
        );
        assert!(
            unattributed > 0,
            "premise: it also has context lines, which must stay bare"
        );

        let painted_authors = (0..flat_row_probe_limit())
            .filter(|row| cx.debug_bounds(sel(format!("diff-author-{row}"))).is_some())
            .count();
        let painted_lines = (0..flat_row_probe_limit())
            .filter(|row| cx.debug_bounds(sel(format!("diff-line-{row}"))).is_some())
            .count();
        assert_eq!(
            painted_authors, attributed,
            "exactly the attributed lines get a bar - no line is guessed at, and none is skipped"
        );
        assert!(
            painted_lines > painted_authors,
            "and the lines nobody is on record for really do render with an empty gutter"
        );

        // "An additional channel, not a replacement": the row still carries its diff-kind accent,
        // and the author bar sits ahead of it.
        let first_authored = (0..flat_row_probe_limit())
            .find(|row| cx.debug_bounds(sel(format!("diff-author-{row}"))).is_some())
            .expect("at least one attributed line is on screen");
        let author_bar = cx
            .debug_bounds(sel(format!("diff-author-{first_authored}")))
            .expect("author bar");
        let kind_bar = cx
            .debug_bounds(sel(format!("diff-kind-{first_authored}")))
            .expect("the diff-kind accent must still be there");
        assert!(
            author_bar.origin.x < kind_bar.origin.x,
            "who wrote it reads first, then what the diff does to it: author bar at {:?}, kind \
             accent at {:?}",
            author_bar.origin.x,
            kind_bar.origin.x
        );
        assert_eq!(
            author_bar.size.width,
            gpui::px(2.0),
            "the mock's own 2px gutter bar"
        );
    }

    #[gpui::test]
    fn a_file_with_no_recorded_provenance_gets_no_gutter_bars_at_all(cx: &mut TestAppContext) {
        let (repo, _store) = shared_file_repo();
        let (_app, cx) = open_changes(
            cx,
            &repo,
            super::super::store::ProvenanceStore::default(),
            &[ProcessKind::claude(), ProcessKind::codex()],
        );

        let row = cx
            .debug_bounds(sel(format!("change-row-{SHARED_PATH}")))
            .expect("the row is still there - the file is still dirty");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("diff-line-0").is_some(),
            "premise: the diff really rendered"
        );
        assert!(
            (0..flat_row_probe_limit())
                .all(|row| cx.debug_bounds(sel(format!("diff-author-{row}"))).is_none()),
            "not one bar is guessed at"
        );
        assert!(
            cx.debug_bounds(RING).is_none(),
            "and with nothing on record, nothing is shared either"
        );
    }

    #[gpui::test]
    fn clicking_the_ring_opens_the_file_filtered_and_names_the_filter(cx: &mut TestAppContext) {
        let (repo, store) = shared_file_repo();
        let (app, cx) = open_changes(
            cx,
            &repo,
            store,
            &[ProcessKind::claude(), ProcessKind::codex()],
        );

        assert!(
            cx.debug_bounds("diff-author-filter").is_none(),
            "at rest there is no indicator at all - not a greyed-out one (STAGE-A-CHANGELOG §4b)"
        );

        let ring = cx.debug_bounds(RING).expect("the ring");
        cx.simulate_click(ring.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change.as_deref(),
                Some(Path::new(SHARED_PATH)),
                "the ring opens the file, from a row whose diff was not on screen yet"
            );
            let filter = app
                .active_author_filter()
                .expect("and filters it to one author");
            assert_eq!(
                *filter,
                chip_authors(
                    app.uncommitted_change_set
                        .entry(Path::new(SHARED_PATH))
                        .expect("row")
                )[0],
                "to the file's first author, which is what the mock's own `pickAttr` picks"
            );
        });
        assert!(
            cx.debug_bounds("diff-author-filter").is_some(),
            "and the toolbar names the filter, so a filtered diff cannot read as the whole diff"
        );
    }

    #[gpui::test]
    fn alt_clicking_a_chip_filters_to_that_chips_own_author(cx: &mut TestAppContext) {
        let (repo, store) = shared_file_repo();
        let (app, cx) = open_changes(
            cx,
            &repo,
            store,
            &[ProcessKind::claude(), ProcessKind::codex()],
        );

        let chip = cx.debug_bounds(CHIP_YOU).expect("the hand-edit chip");
        cx.simulate_click(
            chip.center(),
            gpui::Modifiers {
                alt: true,
                ..Default::default()
            },
        );
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.active_author_filter(),
                Some(&Author::You),
                "the third chip, not the row's first author"
            );
        });
    }

    #[gpui::test]
    fn dismissing_the_indicator_clears_the_filter(cx: &mut TestAppContext) {
        let (repo, store) = shared_file_repo();
        let (app, cx) = open_changes(
            cx,
            &repo,
            store,
            &[ProcessKind::claude(), ProcessKind::codex()],
        );

        let ring = cx.debug_bounds(RING).expect("the ring");
        cx.simulate_click(ring.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let indicator = cx
            .debug_bounds("diff-author-filter")
            .expect("premise: a filter is in force");
        assert!(
            cx.debug_bounds("diff-author-filter-clear").is_some(),
            "the indicator carries its own ✕"
        );
        cx.simulate_click(indicator.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(app.active_author_filter(), None);
        });
        assert!(
            cx.debug_bounds("diff-author-filter").is_none(),
            "and the control that acts on a filter does not exist when there is none"
        );
    }

    #[gpui::test]
    fn opening_another_file_is_not_still_filtered(cx: &mut TestAppContext) {
        let (repo, store) = shared_file_repo();
        std::fs::write(repo.path().join("other.rs"), "fn other() {}\n")
            .expect("dirty a second file");
        let (app, cx) = open_changes(
            cx,
            &repo,
            store,
            &[ProcessKind::claude(), ProcessKind::codex()],
        );

        let ring = cx.debug_bounds(RING).expect("the ring");
        cx.simulate_click(ring.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(app.active_author_filter().is_some(), "premise: filtered");
        });

        let other = cx
            .debug_bounds("change-row-other.rs")
            .expect("the second file's row");
        cx.simulate_click(other.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(app.open_change.as_deref(), Some(Path::new("other.rs")));
            assert_eq!(
                app.active_author_filter(),
                None,
                "a filter entered on one file must not silently dim another"
            );
        });
        assert!(cx.debug_bounds("diff-author-filter").is_none());
    }

    #[gpui::test]
    fn the_graph_working_tree_row_carries_the_union_not_one_agent(cx: &mut TestAppContext) {
        let (repo, store) = shared_file_repo();
        let (app, cx) = open_changes(
            cx,
            &repo,
            store,
            &[ProcessKind::claude(), ProcessKind::codex()],
        );
        app.update_in(cx, |app, window, cx| app.open_git_graph(window, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let crate::graph_view::state::GraphLoadState::Loaded(graph) = &app.graph_state.load
            else {
                panic!("premise: the graph really loaded");
            };
            let first = graph.rows.first().expect("premise: it has rows");
            assert!(
                first.commit.id.is_empty(),
                "premise: the first row is the synthetic working-tree row"
            );
            assert_eq!(
                first.commit.subject, "Working tree",
                "I5's rename - `Uncommitted changes` named the panel's other surface"
            );
            assert!(
                first.commit.author_name.is_empty(),
                "and it pins no single author of its own"
            );
        });

        assert!(
            cx.debug_bounds("graph-working-tree-authors").is_some(),
            "it carries the contributing agents instead"
        );
        for chip in [
            "graph-working-tree-authors-claude",
            "graph-working-tree-authors-codex",
            "graph-working-tree-authors-you",
        ] {
            assert!(
                cx.debug_bounds(chip).is_some(),
                "{chip} must be part of the `by` union"
            );
        }
        assert!(
            cx.debug_bounds("graph-working-tree-note").is_some(),
            "with the real union figure beside them"
        );
    }

    /// How far down the flat diff row list these tests probe. The fixture's diff is a handful of
    /// lines; this is comfortably past its end, and probing a row that does not exist is simply a
    /// `None` from `debug_bounds`.
    fn flat_row_probe_limit() -> usize {
        64
    }

    /// `VisualTestContext::debug_bounds` takes a `&'static str`, so a per-path or per-row selector
    /// has to outlive the call - the same `Box::leak` every other selector-building test helper in
    /// this crate uses (`crate::rail::menu_render`'s own `worktree_row_selector`).
    fn sel(selector: String) -> &'static str {
        Box::leak(selector.into_boxed_str())
    }
}
