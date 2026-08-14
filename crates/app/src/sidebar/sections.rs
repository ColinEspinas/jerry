//! Pure logic for the Changes panel's four stacked sections (GitHub issue #285).
//!
//! `design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §1: the right panel's single flat
//! change list becomes **four collapsible sections in one scroller**, each with its own count and
//! diffstat, the commit box pinned above them.
//!
//! > Sections, not a segmented picker: triage needs to see that there are uncommitted changes
//! > *and* three commits *and* a run waiting, without operating a control.
//!
//! ## The four scopes are four different questions, not one list filtered four ways
//!
//! | | **Runs** | **Uncommitted** | **Commits** | **Against main** |
//! |---|---|---|---|---|
//! | Answers | what *this agent* did in *this run* | what is dirty in the checkout | what is written down | what would land |
//! | Backed by | the provenance union's per-author `split` (`crate::provenance::change_set`) | `wt_core::diff::diff_against_head` | `wt_core::diff::commits_since_base` | `wt_core::diff::diff_against_base` |
//! | Checkboxes | **never** | yes | no | no |
//!
//! The middle row is the load-bearing one. Before this issue the panel showed exactly one of
//! these (the merge-base diff) and called it "Changes", so committed and uncommitted work sat
//! intermixed in one list and none of the four questions was answerable at a glance.
//! `diff_against_head` is a genuinely new scope added for this (see its own docs for why it is not
//! a filtered view of `diff_against_base`).
//!
//! ## Why Runs sums to Uncommitted
//!
//! `STAGE-A-CHANGELOG.md` §5 records the mock hitting this exact problem and the fix:
//!
//! > Rather than duplicate rows (which would reintroduce the double-staging defect the 07-31 rule
//! > exists to prevent), run rows now derive from the **union**, taking each agent's share from a
//! > new `split` field on shared rows. […] Runs now sums to Uncommitted by construction.
//!
//! [`run_rows`] does exactly that and nothing else: a run's file count and diffstat are read
//! straight off [`crate::provenance::change_set::ChangeSet`]'s own per-author partition of the
//! **uncommitted** diff. There is no second count for the two to disagree about, so the Runs
//! header's total is the agents' share of the Uncommitted header's total by construction - equal
//! to it exactly when every uncommitted line is some agent's, and short of it by precisely the
//! human's own and the unattributed lines otherwise. Both properties are pinned by this module's
//! own tests.
//!
//! ## Self-labelling (audit I6)
//!
//! Every section states its own base: [`ChangesSection::scope_phrase`] is rendered as the header's
//! tooltip, so all five entry points that land in this panel arrive somewhere that declares its
//! scope rather than at an unlabelled list.

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
///
/// Mirrors `crate::code_surface::state::DiffLoadState`'s and
/// `crate::review::state::ReviewLoadState`'s shape, for one consistent idiom across this crate's
/// background-loaded surfaces. The distinction that matters is the same one they document:
/// [`ScopeLoad::Error`] is a real message from the underlying `wt_core` call, surfaced rather than
/// swallowed into an empty result - which would read as "nothing changed", the single most
/// misleading thing a Changes section could say.
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
///
/// The variant order is the render order, and [`Self::ORDER`] is the one place it is written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChangesSection {
    Uncommitted,
    Commits,
    AgainstMain,
    Runs,
}

impl ChangesSection {
    /// Top to bottom, exactly as `Jerry.dc.html` both paints them (`onSecUnc` at line 1314,
    /// `onSecCommits` at 1370, `onSecBase` at 1390, `onSecRuns` last at 1434) and says so in its own
    /// comment: "Four stacked sections, in this order: Uncommitted, Commits, Against main, Runs.
    /// The first three are one ladder of git state, narrowing to widening. Runs is not on that
    /// ladder — it indexes the same changes by author — so it sits after it rather than inside it,
    /// which also keeps Uncommitted's top edge fixed however many agents have run."
    ///
    /// `REVISION-2026-08-14.md` §1's own sketch lists Runs first; the mock is the more authoritative
    /// of the two design sources and is unambiguous (comment and paint order agree), so this follows
    /// the mock.
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
    ///
    /// Uppercased here rather than by the renderer because GPUI has no `text-transform`: the
    /// string a test reads back and the string that is painted have to be the same string.
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

    /// `REVISION-2026-08-14.md` §1: "Runs and Uncommitted open by default; the two git-history
    /// sections start collapsed."
    pub fn starts_open(self) -> bool {
        match self {
            ChangesSection::Runs | ChangesSection::Uncommitted => true,
            ChangesSection::Commits | ChangesSection::AgainstMain => false,
        }
    }

    /// The header tooltip that makes this section self-labelling (audit I6) - it states the
    /// section's **base point**, which is the fact that tells the four scopes apart.
    pub fn scope_phrase(self, base_branch: Option<&str>) -> String {
        match self {
            ChangesSection::Runs => {
                "What each agent wrote in this worktree, from the uncommitted changes attributed \
                 to it"
                    .to_string()
            }
            ChangesSection::Uncommitted => {
                "Everything dirty in this checkout, whoever wrote it - working tree \u{2192} HEAD"
                    .to_string()
            }
            ChangesSection::Commits => match base_branch {
                Some(base) => format!("Work already written down on this branch, since {base}"),
                None => "Work already written down on this branch".to_string(),
            },
            ChangesSection::AgainstMain => match base_branch {
                Some(base) => format!("What this branch would land on {base}"),
                None => "What this branch would land on its base".to_string(),
            },
        }
    }

    /// The section's 2px left edge (`REVISION-2026-08-14.md` §1's table).
    ///
    /// `None` for [`ChangesSection::Runs`]: a run's edge is *its own agent's tint*, resolved per
    /// row, which is the entire point of per-agent attribution - see [`RunRow::tint_fg`].
    pub fn edge_color(self) -> Option<Rgba> {
        match self {
            ChangesSection::Runs => None,
            ChangesSection::Uncommitted => Some(theme::changes::EDGE_UNCOMMITTED.into()),
            ChangesSection::Commits => Some(theme::changes::EDGE_NEUTRAL.into()),
            ChangesSection::AgainstMain => Some(theme::changes::EDGE_AGAINST_MAIN.into()),
        }
    }

    /// Whether rows in this section carry a staging checkbox.
    ///
    /// `REVISION-2026-08-14.md` §9, box 1: "checkboxes only on Uncommitted". A checkbox in any
    /// other section would be a control acting outside its own scope - staging a commit, or
    /// staging one agent's share of a file the other agent also wrote.
    pub fn has_checkboxes(self) -> bool {
        matches!(self, ChangesSection::Uncommitted)
    }
}

/// Which sections are open right now.
///
/// Absence means "never toggled", which falls through to [`ChangesSection::starts_open`] rather
/// than to a blanket default - so the two git-history sections really do start collapsed, and a
/// section the user has explicitly collapsed stays collapsed even though that matches the default.
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

/// One rendered run row (`REVISION-2026-08-14.md` §1 + `STAGE-A-CHANGELOG.md` §4l).
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
    /// The row tooltip. `STAGE-A-CHANGELOG.md` §4l dropped the `live`/`frozen` words from the row
    /// itself ("it restated `running`/`ended` one line below"); the tooltip is where the
    /// *consequence* still gets said.
    pub tooltip: &'static str,
}

impl RunRow {
    /// Line 2's left-hand colour - warm while the run is still moving, neutral once it has ended
    /// (`STAGE-A-CHANGELOG.md` §4l).
    pub fn meta_color(&self) -> Rgba {
        if self.live {
            theme::changes::RUN_META_LIVE.into()
        } else {
            theme::changes::RUN_META_ENDED.into()
        }
    }
}

/// A live run's diff is not final; an ended run's is not going to grow.
const LIVE_RUN_TOOLTIP: &str = "Still running - this run's diff is not final yet";
const ENDED_RUN_TOOLTIP: &str =
    "This run has ended - nothing further will be attributed to it, so its diff is frozen";

/// Builds the Runs section's rows from the **uncommitted** change set's own per-author partition.
///
/// Every number on a row is read off `change_set`, never counted a second time here, which is what
/// makes the section header's total the agents' share of the Uncommitted header's total by
/// construction rather than by agreement (see this module's own docs, and
/// [`runs_sum_to_the_uncommitted_total_when_every_line_is_an_agent_s`]).
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
                tooltip: if source.live {
                    LIVE_RUN_TOOLTIP
                } else {
                    ENDED_RUN_TOOLTIP
                },
            }
        })
        .collect()
}

/// Line 1 of a run row: the files this run wrote, newest-listed-first in the diff's own order.
///
/// **This app records no per-run task title, and this deliberately does not invent one.** The mock
/// puts a sentence here (`Extract query builder from the ORM layer`) because its runs are authored
/// demo data; nothing in this codebase captures a run's brief - not the hook layer
/// (`crate::hooks::event` reports tool calls and notifications, never the prompt), not
/// `crate::hooks::store` (status, activity and question only), not the terminal title. A plausible
/// invented sentence in the most prominent line of the row would be the single worst place in this
/// panel to put fiction.
///
/// So line 1 answers the section's own question - *what did this agent do in this run* - with the
/// real answer the app actually has: the paths, from the very same change-set partition the row's
/// counts come from. `no files yet` when the run has written nothing, which is a real state (a run
/// that has only read, or has not written yet), not an error.
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

/// Line 2's left-hand text: `<agent> · ended 2m`, or `<agent> · running · 40s` while it is still
/// moving (`STAGE-A-CHANGELOG.md` §4l's exact shape).
pub fn run_meta(agent_label: &str, live: bool, elapsed: Duration) -> String {
    let age = crate::rail::state::format_elapsed(elapsed);
    if live {
        format!("{agent_label} \u{b7} running \u{b7} {age}")
    } else {
        format!("{agent_label} \u{b7} ended {age}")
    }
}

/// A section header's right-aligned diffstat, split into its two coloured halves.
///
/// `None` for a section with nothing in it: `STAGE-A-CHANGELOG.md`'s own headers render an empty
/// diffstat rather than `+0 −0`, and §7 rule 2 generalises it - "a control that acts on results
/// does not exist when there are none".
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
///
/// `theme::changes`' own docs state the semantics this exists to honour: *"The state these encode
/// is **'seen since the agent last changed it'**, not 'opened once': a file you read and the agent
/// then edits again reverts to unseen."* Storing the diffstat the file had when it was opened is
/// what makes that real - if the file's stat has moved since, something has changed it since you
/// looked, so it is unseen again. Deliberately not a bare `HashSet<PathBuf>`, which could only ever
/// encode "opened once".
///
/// This is the `reviewed` half of `REVISION-2026-08-14.md` §1's rule 2 ("**`reviewed` and `staged`
/// are separate fields.** Reviewing must never stage."). It is a wholly separate map from
/// `crate::root::AdeApp::staged_files`, read by nothing that stages and written by nothing that
/// stages.
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
///
/// The panel is one scroller holding genuinely different row heights - a 24px section header, a
/// 27px file row, a 48px two-line run row - which is precisely what `gpui::uniform_list` cannot
/// represent (it sizes every slot from item 0). So the list is `gpui::list`/`gpui::ListState`,
/// GPUI's own variable-height virtualized list, and this is its item model. It is still real
/// virtualization: a row scrolled far below the viewport is never built at all, which
/// `crate::sidebar::render`'s own `virtualization_tests` assert against a live render.
#[derive(Debug, Clone, PartialEq)]
pub enum SectionRow {
    Header(SectionHeader),
    Run(RunRow),
    /// An index into the **uncommitted** diff's file list, re-resolved (never captured) at build
    /// time so a diff replaced between this frame's row count and this row's build renders
    /// nothing rather than indexing a stale snapshot.
    UncommittedFile(usize),
    Commit(wt_core::diff::BranchCommit),
    /// An index into the merge-base diff's file list, same discipline.
    AgainstMainFile(usize),
    /// The Against-main section's read-only context card - what would land, and how far ahead or
    /// behind the branch is. Deliberately **not** a file row: the header's count is the file
    /// count, so counting this would break "header count equals rendered row count".
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
    ///
    /// Headers, the Against-main context card and section notes are not: counting them would break
    /// the panel's own acceptance criterion that a header's count equals the number of rows it
    /// renders. See [`SectionHeader::count`], which is derived by running this over the section's
    /// own body rather than by a second, independent count.
    pub fn is_counted(&self) -> bool {
        matches!(
            self,
            SectionRow::Run(_)
                | SectionRow::UncommittedFile(_)
                | SectionRow::Commit(_)
                | SectionRow::AgainstMainFile(_)
        )
    }
}

/// One section's header row, with everything it states already resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionHeader {
    pub section: ChangesSection,
    /// [`ChangesSection::label`]'s output, already uppercased and already carrying the real base
    /// branch name where that applies.
    pub label: String,
    /// **Derived from the section's own body rows**, by counting the ones
    /// [`SectionRow::is_counted`] accepts - so "the header count equals the rendered row count" is
    /// a property of how the list is built rather than an agreement between two counters. The body
    /// is built whether or not the section is open, and only *pushed* when it is, which is what
    /// lets a collapsed section still state a true count.
    pub count: usize,
    pub stat: DiffStat,
    pub open: bool,
    /// [`ChangesSection::scope_phrase`]'s output - the header's tooltip, and the thing that makes
    /// the section self-labelling (audit I6).
    pub scope: String,
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
mod tests {
    use super::*;
    use crate::provenance::change_set::build_change_set;
    use crate::provenance::store::ProvenanceStore;
    use std::process::Command;
    use wt_core::diff::diff_against_head;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

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
            git(repo, &["init", "-b", "main"]);
            git(repo, &["config", "user.email", "test@example.com"]);
            git(repo, &["config", "user.name", "Test User"]);
            std::fs::write(repo.join("shared.rs"), "one\ntwo\nthree\n").expect("seed shared");
            std::fs::write(repo.join("solo.rs"), "alpha\n").expect("seed solo");
            git(repo, &["add", "-A"]);
            git(repo, &["commit", "-m", "initial"]);

            let mut store = ProvenanceStore::default();
            let s3 = AgentKey::new("utf8:/repo/wt-a|Claude|1700000000");
            let s10 = AgentKey::new("utf8:/repo/wt-a|Claude|1700000900");

            // s3 rewrites the first line, s10 the last - one path, two agents.
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
        // `STAGE-A-CHANGELOG.md` §3's own verification of the mock: "Runs `+319 −145` and
        // Uncommitted `+319 −145` agree exactly." Here, against a real git repo and a real
        // provenance store rather than authored demo data.
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
        // `REVISION-2026-08-14.md` §1 rule 1, seen from the Runs side: the per-run file counts
        // deliberately over-sum the worktree's row count, and that is what "they are both working
        // on this file" means.
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
    fn the_order_matches_the_mock_not_the_issue_sketch() {
        // `Jerry.dc.html` paints `onSecUnc`, `onSecCommits`, `onSecBase`, then `onSecRuns` last,
        // and says so in its own comment - "Runs is not on [the git-state] ladder ... so it sits
        // after it rather than inside it, which also keeps Uncommitted's top edge fixed however
        // many agents have run." `REVISION-2026-08-14.md` §1's sketch lists Runs first; the mock
        // wins the disagreement.
        assert_eq!(
            ChangesSection::ORDER,
            [
                ChangesSection::Uncommitted,
                ChangesSection::Commits,
                ChangesSection::AgainstMain,
                ChangesSection::Runs,
            ]
        );
    }

    #[test]
    fn a_runs_row_never_has_a_checkbox_and_only_uncommitted_does() {
        // `REVISION-2026-08-14.md` §9, box 1.
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
    fn every_section_states_its_own_base_point() {
        // Audit I6: every entry point that lands in this panel arrives somewhere that states its
        // scope.
        for section in ChangesSection::ORDER {
            let phrase = section.scope_phrase(Some("main"));
            assert!(!phrase.is_empty(), "{} has no scope phrase", section.key());
        }
        assert!(ChangesSection::Uncommitted
            .scope_phrase(None)
            .contains("HEAD"));
        assert!(ChangesSection::AgainstMain
            .scope_phrase(Some("main"))
            .contains("main"));
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
    fn a_live_run_and_an_ended_run_render_side_by_side_with_their_own_meta_and_tooltip() {
        // The mock's sad path (`STAGE-A-SELFCHECK.md`): a live run and a frozen run in the Runs
        // section at once, both readable in one screen.
        let fixture = Fixture::two_agents_wrote_everything();
        let rows = run_rows(&fixture.sources(), &fixture.change_set());

        assert_eq!(rows.len(), 2);
        assert!(!rows[0].live);
        assert_eq!(rows[0].meta, "Claude \u{b7} ended 2m");
        assert_eq!(rows[0].meta_color(), theme::changes::RUN_META_ENDED.into());
        assert!(rows[0].tooltip.contains("frozen"));

        assert!(rows[1].live);
        assert_eq!(rows[1].meta, "Claude \u{b7} running \u{b7} 40s");
        assert_eq!(
            rows[1].meta_color(),
            theme::changes::RUN_META_LIVE.into(),
            "a live run's meta renders warm"
        );
        assert!(rows[1].tooltip.contains("not final"));
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
        // `REVISION-2026-08-14.md` §1 rule 2: "Reviewing must never stage." Structurally true
        // here - `SeenFiles` is its own map with its own type, and the staged set is a
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
