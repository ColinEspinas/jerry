//! Pure logic for the Changes panel's four stacked sections (GitHub issue #285).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::Rgba;

use crate::provenance::change_set::ChangeSet;
use crate::provenance::{AgentKey, Author, DiffStat};
use crate::root::plural;
use crate::theme;
use crate::work_surface::agents::AgentId;

/// A background-loaded scope's three real outcomes, plus not-yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ScopeLoad<T> {
    #[default]
    Loading,
    Loaded(T),
    Error(String),
}

impl<T> ScopeLoad<T> {
    pub fn loaded(&self) -> Option<&T> {
        match self {
            ScopeLoad::Loaded(value) => Some(value),
            ScopeLoad::Loading | ScopeLoad::Error(_) => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            ScopeLoad::Error(message) => Some(message),
            ScopeLoad::Loading | ScopeLoad::Loaded(_) => None,
        }
    }
}

/// One of the Changes panel's four sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChangesSection {
    Uncommitted,
    Commits,
    AgainstMain,
    Runs,
}

impl ChangesSection {
    /// Top to bottom, exactly as the design paints them and states in as many words:
    /// "Four stacked sections, in this order: Uncommitted, Commits, Against main, Runs.
    /// The first three are one ladder of git state, narrowing to widening. Runs is not on that
    /// ladder — it indexes the same changes by author — so it sits after it rather than inside it,
    /// which also keeps Uncommitted's top edge fixed however many agents have run."
    pub const ORDER: [ChangesSection; 4] = [
        ChangesSection::Uncommitted,
        ChangesSection::Commits,
        ChangesSection::AgainstMain,
        ChangesSection::Runs,
    ];

    /// The stable identifier this section's collapse state is filed under, and the prefix its
    /// rendered elements' ids/selectors use. Written by hand rather than derived from the variant
    /// name, so renaming a variant cannot silently re-key anything.
    pub fn key(self) -> &'static str {
        match self {
            ChangesSection::Runs => "runs",
            ChangesSection::Uncommitted => "uncommitted",
            ChangesSection::Commits => "commits",
            ChangesSection::AgainstMain => "against-main",
        }
    }

    /// The header's uppercase label. `Against main` names the real detected base branch, so it is
    /// the one label that depends on git state - `Against base` when no base branch was detected,
    /// never a hardcoded `main` the repository might not have.
    pub fn label(self, base_branch: Option<&str>) -> String {
        match self {
            ChangesSection::Runs => "RUNS".to_string(),
            ChangesSection::Uncommitted => "UNCOMMITTED".to_string(),
            ChangesSection::Commits => "COMMITS".to_string(),
            ChangesSection::AgainstMain => match base_branch {
                Some(base) => format!("AGAINST {}", base.to_uppercase()),
                None => "AGAINST BASE".to_string(),
            },
        }
    }

    /// Runs and Uncommitted open by default; the two git-history sections start collapsed.
    pub fn starts_open(self) -> bool {
        match self {
            ChangesSection::Runs | ChangesSection::Uncommitted => true,
            ChangesSection::Commits | ChangesSection::AgainstMain => false,
        }
    }

    /// The section's 2px left edge.
    pub fn edge_color(self) -> Option<Rgba> {
        match self {
            ChangesSection::Runs => None,
            ChangesSection::Uncommitted => Some(theme::changes::EDGE_UNCOMMITTED.into()),
            ChangesSection::Commits => Some(theme::changes::EDGE_NEUTRAL.into()),
            ChangesSection::AgainstMain => Some(theme::changes::EDGE_AGAINST_MAIN.into()),
        }
    }

    /// Whether rows in this section carry a staging checkbox.
    pub fn has_checkboxes(self) -> bool {
        matches!(self, ChangesSection::Uncommitted)
    }
}

/// Which sections are open right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SectionCollapse {
    overrides: HashMap<ChangesSection, bool>,
}

impl SectionCollapse {
    pub fn is_open(&self, section: ChangesSection) -> bool {
        self.overrides
            .get(&section)
            .copied()
            .unwrap_or_else(|| section.starts_open())
    }

    pub fn toggle(&mut self, section: ChangesSection) {
        let next = !self.is_open(section);
        self.overrides.insert(section, next);
    }
}

/// One run, as input to [`run_rows`] - everything about a live agent that the pure row model
/// needs, read off `crate::work_surface::agents::Agent` by the caller (which owns the `App` the
/// terminal pane's state has to be read through).
#[derive(Debug, Clone, PartialEq)]
pub struct RunSource {
    pub agent_id: AgentId,
    /// The durable identity this run's lines are attributed under -
    /// `crate::review::state::baseline_key`, the same key `crate::provenance` files them by. This
    /// is the join, and it is why the Runs section reuses the review-baseline machinery's identity
    /// rather than inventing a second one.
    pub agent_key: AgentKey,
    /// The agent CLI's own name (`Claude`, `Codex`).
    pub agent_label: String,
    /// The agent tint chip's single letter.
    pub initial: &'static str,
    /// `(foreground, background)` from `crate::work_surface::state::agent_tint`.
    pub tint: (Rgba, Rgba),
    /// Whether the run is still moving - a live process, not one that has exited.
    pub live: bool,
    /// How long the run has been going (live) or how long since it last did anything (ended).
    pub elapsed: Duration,
}

/// One rendered run row.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub agent_id: AgentId,
    /// Line 1, full width. See [`run_title`] for why this is the run's file list and not a task
    /// title.
    pub title: String,
    /// Line 2, left: `<agent> · ended 2m` or `<agent> · running · 40s`.
    pub meta: String,
    /// Line 2, right: `12 files`.
    pub files_label: String,
    pub initial: &'static str,
    pub tint_fg: Rgba,
    pub tint_bg: Rgba,
    /// The row's own left edge - this run's agent tint, always painted.
    pub live: bool,
    pub stat: DiffStat,
}

impl RunRow {
    /// Line 2's left-hand colour - warm while the run is still moving, neutral once it has
    /// ended.
    pub fn meta_color(&self) -> Rgba {
        if self.live {
            theme::changes::RUN_META_LIVE.into()
        } else {
            theme::changes::RUN_META_ENDED.into()
        }
    }
}

/// Builds the Runs section's rows from the **uncommitted** change set's own per-author partition.
pub fn run_rows(sources: &[RunSource], change_set: &ChangeSet) -> Vec<RunRow> {
    sources
        .iter()
        .map(|source| {
            let author = Author::Agent(source.agent_key.clone());
            let paths = change_set.paths_for(&author);
            let stat = change_set.split().get(&author).copied().unwrap_or_default();
            RunRow {
                agent_id: source.agent_id,
                title: run_title(&paths),
                meta: run_meta(&source.agent_label, source.live, source.elapsed),
                files_label: plural::count(paths.len(), "file", None),
                initial: source.initial,
                tint_fg: source.tint.0,
                tint_bg: source.tint.1,
                live: source.live,
                stat,
            }
        })
        .collect()
}

/// Line 1 of a run row: the files this run wrote, newest-listed-first in the diff's own order.
pub fn run_title(paths: &[&Path]) -> String {
    const NAMED: usize = 2;
    let names: Vec<String> = paths
        .iter()
        .take(NAMED)
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        })
        .collect();
    match (names.len(), paths.len()) {
        (0, _) => "no files yet".to_string(),
        (_, total) if total > names.len() => {
            format!("{} and {} more", names.join(", "), total - names.len())
        }
        _ => names.join(", "),
    }
}

/// Line 2's left-hand text: `<agent> · ended 2m`, or `<agent> · running · 40s` while it is
/// still moving.
pub fn run_meta(agent_label: &str, live: bool, elapsed: Duration) -> String {
    let age = crate::rail::state::format_elapsed(elapsed);
    if live {
        format!("{agent_label} \u{b7} running \u{b7} {age}")
    } else {
        format!("{agent_label} \u{b7} ended {age}")
    }
}

/// A section header's right-aligned diffstat, split into its two coloured halves.
pub fn section_diffstat(stat: DiffStat) -> Option<(String, String)> {
    if stat.is_empty() {
        return None;
    }
    Some((
        format!("+{}", stat.added),
        format!("\u{2212}{}", stat.removed),
    ))
}

/// The Uncommitted header's seen counter - `3/13 seen`, or `None` with nothing uncommitted.
pub fn seen_label(seen: usize, total: usize) -> Option<String> {
    if total == 0 {
        return None;
    }
    Some(format!("{seen}/{total} seen"))
}

/// How full the seen meter beside [`seen_label`] is, as `0.0..=1.0`. `0.0` (not `NaN`) with
/// nothing to have seen.
pub fn seen_fraction(seen: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        (seen as f32 / total as f32).clamp(0.0, 1.0)
    }
}

/// What was true about a file the moment it was marked seen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeenFiles {
    /// Keyed by worktree root, then by repo-relative path - so switching worktrees and back does
    /// not silently lose what you had already read, and one worktree's progress never leaks into
    /// another's counter.
    marks: HashMap<PathBuf, HashMap<PathBuf, DiffStat>>,
}

impl SeenFiles {
    /// Records `path` as seen at the diffstat it currently has.
    pub fn mark_seen(&mut self, worktree: &Path, path: &Path, stat: DiffStat) {
        self.marks
            .entry(worktree.to_path_buf())
            .or_default()
            .insert(path.to_path_buf(), stat);
    }

    /// Drops `path`'s mark entirely - the explicit "unmark" gesture, distinct from a mark going
    /// stale because the file moved under it.
    pub fn clear(&mut self, worktree: &Path, path: &Path) {
        if let Some(worktree_marks) = self.marks.get_mut(worktree) {
            worktree_marks.remove(path);
        }
    }

    /// Whether `path` has been seen **since it last changed**: it was marked, and it still has the
    /// diffstat it had when it was marked.
    pub fn is_seen(&self, worktree: &Path, path: &Path, stat: DiffStat) -> bool {
        self.marks
            .get(worktree)
            .and_then(|worktree_marks| worktree_marks.get(path))
            .is_some_and(|marked| *marked == stat)
    }

    /// How many of `change_set`'s rows are seen right now - the Uncommitted header's numerator.
    pub fn seen_count(&self, worktree: &Path, change_set: &ChangeSet) -> usize {
        change_set
            .entries()
            .iter()
            .filter(|entry| self.is_seen(worktree, &entry.path, entry.stat()))
            .count()
    }
}

/// One row of the panel's single scroller, flattened across all four sections.
#[derive(Debug, Clone, PartialEq)]
pub enum SectionRow {
    Header(SectionHeader),
    Run(RunRow),
    /// An index into the **uncommitted** diff's file list, re-resolved (never captured) at build
    /// time so a diff replaced between this frame's row count and this row's build renders
    /// nothing rather than indexing a stale snapshot.
    UncommittedFile(usize),
    Commit(wt_core::diff::BranchCommit),
    /// The Against-main section's *only* body row (besides its own notes): what would land, and
    /// how far ahead or behind the branch is. Unlike the other three sections, Against main never
    /// lists a row per file - the design carries a plain file *count* here, never an array of
    /// files, and the panel's
    /// own header count reads that count directly (`Self::changes_section_rows`), not the number
    /// of rows this section renders - a deliberate exception to `SectionHeader::count`'s usual
    /// "derived from the body" rule, and a deliberate removal of this section's earlier per-file
    /// listing (a committed file is no longer its own row here at all).
    AgainstMainContext {
        text: String,
        sub: String,
    },
    /// A section's own message - empty, still loading, failed, or truncated. Also not a file row.
    Note {
        section: ChangesSection,
        text: String,
        emphasis: NoteEmphasis,
    },
}

impl SectionRow {
    /// Whether this row is one of the things its section's header *counts*.
    pub fn is_counted(&self) -> bool {
        matches!(
            self,
            SectionRow::Run(_) | SectionRow::UncommittedFile(_) | SectionRow::Commit(_)
        )
    }

    /// Which of the four sections this row belongs to - what
    /// `Self::render_changes_sections`/`Self::render_changes_runs_section` (in `sidebar::render`)
    /// split the flattened `changes_section_rows` list on to give Runs its own pinned-bottom
    /// scroller instead of sharing the other three's.
    pub fn section(&self) -> ChangesSection {
        match self {
            SectionRow::Header(header) => header.section,
            SectionRow::Run(_) => ChangesSection::Runs,
            SectionRow::UncommittedFile(_) => ChangesSection::Uncommitted,
            SectionRow::Commit(_) => ChangesSection::Commits,
            SectionRow::AgainstMainContext { .. } => ChangesSection::AgainstMain,
            SectionRow::Note { section, .. } => *section,
        }
    }
}

/// One section's header row, with everything it states already resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionHeader {
    pub section: ChangesSection,
    /// [`ChangesSection::label`]'s output, already uppercased and already carrying the real base
    /// branch name where that applies.
    pub label: String,
    /// For Uncommitted, Commits and Runs: **derived from the section's own body rows**, by
    /// counting the ones [`SectionRow::is_counted`] accepts - so "the header count equals the
    /// rendered row count" is a property of how the list is built rather than an agreement
    /// between two counters. The body is built whether or not the section is open, and only
    /// *pushed* when it is, which is what lets a collapsed section still state a true count.
    pub count: usize,
    pub stat: DiffStat,
    pub open: bool,
    /// `(seen, total)` for the Uncommitted section only - the `N/M seen` counter and its meter.
    /// `None` everywhere else: the other three sections have nothing to have read.
    pub seen: Option<(usize, usize)>,
}

/// How loud a [`SectionRow::Note`] is - a plain "nothing here" versus a real failure or a
/// truncation the user needs to know changes what the numbers above mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteEmphasis {
    Quiet,
    Warning,
}

impl NoteEmphasis {
    pub fn color(self) -> Rgba {
        match self {
            NoteEmphasis::Quiet => theme::text::FAINT.into(),
            NoteEmphasis::Warning => theme::status::ASK.into(),
        }
    }
}

#[cfg(test)]
mod changes_section_tests {
    use super::*;
    use crate::provenance::change_set::build_change_set;
    use crate::provenance::store::ProvenanceStore;
    use test_support::{git, seed_empty_repo_at};
    use wt_core::diff::diff_against_head;

    fn agent_writes(
        store: &mut ProvenanceStore,
        worktree: &Path,
        relative: &str,
        agent: &AgentKey,
        content: &str,
    ) {
        let file = worktree.join(relative);
        store.begin_agent_edit(worktree, &file);
        std::fs::write(&file, content).expect("write");
        store.record_agent_edit(worktree, &file, agent);
    }

    struct Fixture {
        dir: tempfile::TempDir,
        store: ProvenanceStore,
        s3: AgentKey,
        s10: AgentKey,
    }

    impl Fixture {
        /// A real two-agent worktree where **every** uncommitted line was written by one of the
        /// two agents - the mock's `Review · uncommitted` shape, reduced to what the arithmetic
        /// needs.
        fn two_agents_wrote_everything() -> Fixture {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = dir.path();
            seed_empty_repo_at(repo);
            std::fs::write(repo.join("shared.rs"), "one\ntwo\nthree\n").expect("seed shared");
            std::fs::write(repo.join("solo.rs"), "alpha\n").expect("seed solo");
            git(repo, &["add", "-A"]);
            git(repo, &["commit", "-m", "initial"]);

            let mut store = ProvenanceStore::default();
            let s3 = AgentKey::new("utf8:/repo/wt-a|Claude|1700000000");
            let s10 = AgentKey::new("utf8:/repo/wt-a|Claude|1700000900");

            agent_writes(&mut store, repo, "shared.rs", &s3, "ONE\ntwo\nthree\n");
            agent_writes(
                &mut store,
                repo,
                "shared.rs",
                &s10,
                "ONE\ntwo\nTHREE\nFOUR\n",
            );
            agent_writes(&mut store, repo, "solo.rs", &s10, "alpha\nbeta\n");

            Fixture {
                dir,
                store,
                s3,
                s10,
            }
        }

        fn change_set(&self) -> ChangeSet {
            let diff = diff_against_head(self.dir.path())
                .expect("diff_against_head")
                .expect("born HEAD");
            build_change_set(&diff, self.store.worktree(self.dir.path()))
        }

        fn sources(&self) -> Vec<RunSource> {
            vec![
                RunSource {
                    agent_id: 1,
                    agent_key: self.s3.clone(),
                    agent_label: "Claude".to_string(),
                    initial: "C",
                    tint: (theme::agent::SONNET.0.into(), theme::agent::SONNET.1.into()),
                    live: false,
                    elapsed: Duration::from_secs(120),
                },
                RunSource {
                    agent_id: 2,
                    agent_key: self.s10.clone(),
                    agent_label: "Claude".to_string(),
                    initial: "C",
                    tint: (theme::agent::HAIKU.0.into(), theme::agent::HAIKU.1.into()),
                    live: true,
                    elapsed: Duration::from_secs(40),
                },
            ]
        }
    }

    #[test]
    fn runs_sum_to_the_uncommitted_total_when_every_line_is_an_agent_s() {
        // Runs `+319 −145` and Uncommitted `+319 −145` must agree exactly - here against a
        // real git repo and a real provenance store rather than authored demo data.
        let fixture = Fixture::two_agents_wrote_everything();
        let change_set = fixture.change_set();
        let rows = run_rows(&fixture.sources(), &change_set);

        let runs_total = rows
            .iter()
            .fold(DiffStat::default(), |total, row| total.plus(row.stat));
        assert_eq!(
            runs_total,
            change_set.total(),
            "every uncommitted line here was written by one of the two agents, so the Runs \
             section's total must be the Uncommitted section's total exactly"
        );
        assert!(
            !runs_total.is_empty(),
            "sanity: the fixture must really have changes, or the equality above is vacuous"
        );
    }

    #[test]
    fn a_hand_edit_is_the_exact_amount_runs_falls_short_of_uncommitted_by() {
        // The other half of the same property: what Runs does *not* cover is precisely the lines
        // no agent wrote - never a rounding gap, and never quietly folded into an agent's share.
        let fixture = Fixture::two_agents_wrote_everything();
        let repo = fixture.dir.path();
        std::fs::write(repo.join("solo.rs"), "alpha\nbeta\nby my own hand\n").expect("hand edit");
        let mut store = fixture.store.clone();
        store.record_hand_edit(repo, &repo.join("solo.rs"));

        let diff = diff_against_head(repo)
            .expect("diff_against_head")
            .expect("born HEAD");
        let change_set = build_change_set(&diff, store.worktree(repo));
        let rows = run_rows(&fixture.sources(), &change_set);

        let runs_total = rows
            .iter()
            .fold(DiffStat::default(), |total, row| total.plus(row.stat));
        let you = change_set
            .split()
            .get(&Author::You)
            .copied()
            .unwrap_or_default();
        assert_eq!(
            you,
            DiffStat::new(1, 0),
            "sanity: the hand edit really is one added line and nothing else"
        );
        assert_eq!(
            runs_total.plus(you),
            change_set.total(),
            "Runs plus the human's own lines must be the whole uncommitted total, with nothing \
             unaccounted for in between"
        );
        assert!(
            runs_total.added < change_set.total().added,
            "and Runs must genuinely be short of the total, not silently claim the hand edit"
        );
    }

    #[test]
    fn a_file_two_agents_wrote_is_one_file_in_each_of_their_runs_and_one_row_in_uncommitted() {
        // Seen from the Runs side: the per-run file counts deliberately over-sum the
        // worktree's row count, and that is what "they are both working on this file" means.
        let fixture = Fixture::two_agents_wrote_everything();
        let change_set = fixture.change_set();
        let rows = run_rows(&fixture.sources(), &change_set);

        assert_eq!(change_set.len(), 2, "two changed paths in this worktree");
        assert_eq!(
            change_set
                .entries()
                .iter()
                .filter(|entry| entry.path == Path::new("shared.rs"))
                .count(),
            1,
            "the shared path is one row, never two - two rows would give it two checkboxes"
        );
        assert_eq!(rows[0].files_label, "1 file", "s3 wrote in the shared file");
        assert_eq!(
            rows[1].files_label, "2 files",
            "s10 wrote in the shared file and its own"
        );
    }

    #[test]
    fn a_runs_row_never_has_a_checkbox_and_only_uncommitted_does() {
        assert!(!ChangesSection::Runs.has_checkboxes());
        assert!(ChangesSection::Uncommitted.has_checkboxes());
        assert!(!ChangesSection::Commits.has_checkboxes());
        assert!(!ChangesSection::AgainstMain.has_checkboxes());
        assert_eq!(
            ChangesSection::ORDER
                .iter()
                .filter(|section| section.has_checkboxes())
                .count(),
            1,
            "exactly one section stages"
        );
    }

    #[test]
    fn runs_and_uncommitted_start_open_and_the_two_git_history_sections_start_collapsed() {
        let collapse = SectionCollapse::default();
        assert!(collapse.is_open(ChangesSection::Runs));
        assert!(collapse.is_open(ChangesSection::Uncommitted));
        assert!(!collapse.is_open(ChangesSection::Commits));
        assert!(!collapse.is_open(ChangesSection::AgainstMain));
    }

    #[test]
    fn collapse_state_is_per_section_and_a_toggle_touches_no_other_section() {
        let mut collapse = SectionCollapse::default();
        collapse.toggle(ChangesSection::Uncommitted);
        assert!(!collapse.is_open(ChangesSection::Uncommitted));
        assert!(
            collapse.is_open(ChangesSection::Runs),
            "collapsing one section must not move another"
        );
        assert!(!collapse.is_open(ChangesSection::Commits));

        collapse.toggle(ChangesSection::Commits);
        assert!(collapse.is_open(ChangesSection::Commits));
        assert!(!collapse.is_open(ChangesSection::Uncommitted));
    }

    #[test]
    fn the_against_main_label_names_the_real_base_branch_never_a_hardcoded_main() {
        assert_eq!(
            ChangesSection::AgainstMain.label(Some("main")),
            "AGAINST MAIN"
        );
        assert_eq!(
            ChangesSection::AgainstMain.label(Some("develop")),
            "AGAINST DEVELOP"
        );
        assert_eq!(
            ChangesSection::AgainstMain.label(None),
            "AGAINST BASE",
            "with no detected base, the header must not claim one"
        );
        assert_eq!(ChangesSection::Runs.label(Some("main")), "RUNS");
    }

    #[test]
    fn a_run_meta_line_reads_running_while_live_and_ended_once_it_is_not() {
        assert_eq!(
            run_meta("Claude", true, Duration::from_secs(40)),
            "Claude \u{b7} running \u{b7} 40s"
        );
        assert_eq!(
            run_meta("Claude", false, Duration::from_secs(120)),
            "Claude \u{b7} ended 2m"
        );
    }

    #[test]
    fn a_live_run_and_an_ended_run_render_side_by_side_with_their_own_meta() {
        // The mock's sad path (`STAGE-A-SELFCHECK.md`): a live run and a frozen run in the Runs
        // section at once, both readable in one screen.
        let fixture = Fixture::two_agents_wrote_everything();
        let rows = run_rows(&fixture.sources(), &fixture.change_set());

        assert_eq!(rows.len(), 2);
        assert!(!rows[0].live);
        assert_eq!(rows[0].meta, "Claude \u{b7} ended 2m");
        assert_eq!(rows[0].meta_color(), theme::changes::RUN_META_ENDED.into());

        assert!(rows[1].live);
        assert_eq!(rows[1].meta, "Claude \u{b7} running \u{b7} 40s");
        assert_eq!(
            rows[1].meta_color(),
            theme::changes::RUN_META_LIVE.into(),
            "a live run's meta renders warm"
        );
        assert_ne!(
            rows[0].meta_color(),
            rows[1].meta_color(),
            "the warm/neutral split is the only thing carrying live-vs-ended on the row itself, \
             so the two colours must genuinely differ"
        );
        assert_ne!(
            rows[0].tint_fg, rows[1].tint_fg,
            "each run's left edge is its own agent's tint"
        );
    }

    #[test]
    fn a_run_title_names_the_files_it_really_wrote_and_says_so_honestly_when_it_wrote_none() {
        assert_eq!(run_title(&[]), "no files yet");
        assert_eq!(
            run_title(&[Path::new("src/db/query_builder.rs")]),
            "query_builder.rs"
        );
        assert_eq!(
            run_title(&[Path::new("src/a.rs"), Path::new("src/b.rs")]),
            "a.rs, b.rs"
        );
        assert_eq!(
            run_title(&[
                Path::new("src/a.rs"),
                Path::new("src/b.rs"),
                Path::new("src/c.rs"),
                Path::new("src/d.rs"),
            ]),
            "a.rs, b.rs and 2 more"
        );
    }

    #[test]
    fn an_empty_section_shows_no_diffstat_rather_than_a_pair_of_zeroes() {
        assert_eq!(section_diffstat(DiffStat::default()), None);
        assert_eq!(
            section_diffstat(DiffStat::new(319, 145)),
            Some(("+319".to_string(), "\u{2212}145".to_string()))
        );
    }

    #[test]
    fn the_seen_counter_reads_over_the_real_row_count() {
        assert_eq!(seen_label(3, 13), Some("3/13 seen".to_string()));
        assert_eq!(seen_label(0, 0), None);
        assert!((seen_fraction(3, 13) - 3.0 / 13.0).abs() < f32::EPSILON);
        assert_eq!(seen_fraction(0, 0), 0.0);
    }

    #[test]
    fn a_file_the_agent_changes_again_after_you_read_it_goes_back_to_unseen() {
        // `theme::changes`' own stated semantics: "seen since the agent last changed it", not
        // "opened once".
        let mut seen = SeenFiles::default();
        let worktree = Path::new("/repo/wt-a");
        let path = Path::new("src/a.rs");

        seen.mark_seen(worktree, path, DiffStat::new(4, 1));
        assert!(seen.is_seen(worktree, path, DiffStat::new(4, 1)));
        assert!(
            !seen.is_seen(worktree, path, DiffStat::new(9, 1)),
            "the agent wrote more since you looked, so you have not seen the file as it now is"
        );

        seen.mark_seen(worktree, path, DiffStat::new(9, 1));
        assert!(seen.is_seen(worktree, path, DiffStat::new(9, 1)));
        seen.clear(worktree, path);
        assert!(!seen.is_seen(worktree, path, DiffStat::new(9, 1)));
    }

    #[test]
    fn one_worktrees_seen_marks_never_leak_into_anothers() {
        let mut seen = SeenFiles::default();
        let path = Path::new("src/a.rs");
        seen.mark_seen(Path::new("/repo/wt-a"), path, DiffStat::new(1, 0));
        assert!(seen.is_seen(Path::new("/repo/wt-a"), path, DiffStat::new(1, 0)));
        assert!(!seen.is_seen(Path::new("/repo/wt-b"), path, DiffStat::new(1, 0)));
    }

    #[test]
    fn marking_a_file_seen_is_recorded_nowhere_a_stager_reads() {
        // Reviewing must never stage. Structurally true here - `SeenFiles` is its own map
        // with its own type, and the staged set is a
        // `HashSet<PathBuf>` this type has no access to - so what this pins is that marking seen
        // is a complete operation that touches nothing else. The live, rendered half of the same
        // rule is asserted in `crate::sidebar::render`'s own tests.
        let fixture = Fixture::two_agents_wrote_everything();
        let change_set = fixture.change_set();
        let mut seen = SeenFiles::default();
        let worktree = fixture.dir.path();

        assert_eq!(seen.seen_count(worktree, &change_set), 0);
        let entry = &change_set.entries()[0];
        seen.mark_seen(worktree, &entry.path, entry.stat());
        assert_eq!(seen.seen_count(worktree, &change_set), 1);
        assert_eq!(
            seen_label(1, change_set.len()),
            Some(format!("1/{} seen", change_set.len()))
        );
    }
}
