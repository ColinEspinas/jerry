//! One row per path, with the authors that wrote it (GitHub issue #284).
//!
//! This is the join between a real `wt_core::diff::WorktreeDiff` and
//! [`super::store::WorktreeProvenance`]. It exists because attribution is only useful attached to
//! the list a human actually reads, and because the design's first "rule that is easy to get
//! wrong" is a rule about *that list*:
//!
//! > **A path appears once per worktree**, however many agents touched it - `by: ['s3','s10']`
//! > with a combined diffstat, never two rows. Two rows give one path two staging checkboxes, let
//! > it be staged twice, and suppress the `⚠` shared-file ring entirely, because nothing then has
//! > two authors.
//! >
//! > — the design's own rule for a shared worktree
//!
//! [`ChangeSet`] enforces that structurally rather than by convention: it is built through a map
//! keyed by path, so a second row for one path is not a thing that can be constructed. A diff
//! that somehow reported the same path twice is *merged* into the one row, not appended after it.
//!
//! ## `split`, and why it sums
//!
//! `STAGE-A-CHANGELOG.md` §5 left this as the open question for the real build:
//!
//! > **Open question for Stage B:** `split` is authored demo data. In the real app it has to come
//! > from the same per-line provenance that feeds the gutter - the run diff and the split are the
//! > same fact counted two ways. Worth stating in the build spec so they can't drift.
//!
//! They cannot drift here, because they are not two computations. [`ChangeSetEntry::stat`] is
//! **defined as** the sum of [`ChangeSetEntry::split`] - there is no independently-counted total
//! to disagree with it. The split itself is a genuine partition of the diff's own lines:
//!
//! - Every **added** line goes to the author of that line, asked of the very same
//!   [`super::store::PathProvenance::author_at`] the gutter will ask (GitHub issue #287).
//! - Every **removed** line goes to the author who removed it, from
//!   [`super::store::RemovalMark`] - the ledger exists precisely because a deleted line has no
//!   surviving line to carry an author.
//! - Anything neither can answer for goes to [`super::Author::Unattributed`], which is a real
//!   bucket in the split rather than a silent shortfall.
//!
//! Each of the diff's lines therefore lands in exactly one bucket, so
//! `entry.split().values().sum() == entry.stat()` holds by construction, and summing that across
//! the change set gives [`ChangeSet::split`] - the per-run diffstats the Runs section needs
//! (GitHub issue #285), guaranteed to add up to [`ChangeSet::total`].
//!
//! ## What the honest arithmetic looks like
//!
//! `REVISION-2026-07-31.md` §4's rule falls straight out of one row per path: a file both agents
//! touched is one row in the worktree's count and one file in *each* agent's count, so
//! [`ChangeSet::file_count_for`] summed over the agents deliberately **exceeds**
//! [`ChangeSet::len`]. That is not double-counting to be fixed; it is what "they are both working
//! on this file" means, and [`ChangeSet::shared_paths`] is the same fact seen from the row's side
//! - the `⚠` ring.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use wt_core::diff::{DiffFile, DiffLineKind, FileChangeStatus, WorktreeDiff};

use super::store::WorktreeProvenance;
use super::{AgentKey, Author, DiffStat};
use crate::sidebar::changes::parse_hunk_new_range;

/// One path, once, with everyone who wrote in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSetEntry {
    /// The path as the diff names it: relative to the worktree root, and for a rename the *new*
    /// path (matching `wt_core::diff::DiffFile::path`).
    pub path: PathBuf,
    pub status: FileChangeStatus,
    /// `true` when git reported this file as binary, in which case it has no lines to attribute
    /// and its stat is legitimately zero - a real changed path with nothing per-line to say about
    /// it, which is different from a path nobody has authored.
    pub is_binary: bool,
    split: BTreeMap<Author, DiffStat>,
}

impl ChangeSetEntry {
    /// The combined diffstat - **defined** as the sum of [`Self::split`], so the two can never
    /// disagree.
    pub fn stat(&self) -> DiffStat {
        self.split
            .values()
            .copied()
            .fold(DiffStat::default(), DiffStat::plus)
    }

    /// Each author's share. Sums to [`Self::stat`].
    pub fn split(&self) -> &BTreeMap<Author, DiffStat> {
        &self.split
    }

    /// This author's share, or a zero stat for an author who wrote nothing here.
    pub fn share(&self, author: &Author) -> DiffStat {
        self.split.get(author).copied().unwrap_or_default()
    }

    /// The de-duplicated author union - the row's `by:` list. In [`Author`]'s own order: agents
    /// first by key, then `you`, then unattributed if any of these lines are.
    pub fn authors(&self) -> Vec<Author> {
        self.split
            .iter()
            .filter(|(_, stat)| !stat.is_empty())
            .map(|(author, _)| author.clone())
            .collect()
    }

    /// Just the agents, in key order.
    pub fn agents(&self) -> Vec<&AgentKey> {
        self.split
            .iter()
            .filter(|(_, stat)| !stat.is_empty())
            .filter_map(|(author, _)| author.agent())
            .collect()
    }

    /// The `⚠` ring's single meaning, verbatim from `REVISION-2026-08-14.md` §1: *"this path has
    /// lines from more than one agent"*.
    ///
    /// Deliberately counts **agents**, not authors: a file one agent wrote and the human then
    /// hand-edited is not a file two agents are fighting over, and lighting the ring for it would
    /// make the ring mean "someone touched this twice", which is every file.
    pub fn is_shared(&self) -> bool {
        self.agents().len() > 1
    }
}

/// Every changed path in one worktree, once each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    entries: Vec<ChangeSetEntry>,
}

impl ChangeSet {
    /// The rows, in the order the diff listed them (git's own path order).
    pub fn entries(&self) -> &[ChangeSetEntry] {
        &self.entries
    }

    pub fn entry(&self, path: &Path) -> Option<&ChangeSetEntry> {
        self.entries.iter().find(|entry| entry.path == path)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The worktree's combined diffstat - the Uncommitted section header's `+N −N`.
    pub fn total(&self) -> DiffStat {
        self.entries
            .iter()
            .map(ChangeSetEntry::stat)
            .fold(DiffStat::default(), DiffStat::plus)
    }

    /// Every author's share across the whole worktree - the Runs section's per-run diffstats
    /// (GitHub issue #285). Sums to [`Self::total`] by construction.
    pub fn split(&self) -> BTreeMap<Author, DiffStat> {
        let mut split: BTreeMap<Author, DiffStat> = BTreeMap::new();
        for entry in &self.entries {
            for (author, stat) in &entry.split {
                *split.entry(author.clone()).or_default() += *stat;
            }
        }
        split
    }

    /// How many files this author wrote in. Deliberately **not** summable to [`Self::len`]: a
    /// shared path counts once for the worktree and once for each agent in it.
    pub fn file_count_for(&self, author: &Author) -> usize {
        self.paths_for(author).len()
    }

    /// The paths this author wrote in, in row order.
    pub fn paths_for(&self, author: &Author) -> Vec<&Path> {
        self.entries
            .iter()
            .filter(|entry| !entry.share(author).is_empty())
            .map(|entry| entry.path.as_path())
            .collect()
    }

    /// Everyone who wrote anything anywhere in this worktree, de-duplicated, in [`Author`]'s own
    /// order - the working-tree graph row's `by` union (GitHub issue #287).
    ///
    /// The union of the rows' own author lists, read off the same partition every row's `+n`/`−n`
    /// is read off, so the graph row and the panel can never name different sets of authors for
    /// one worktree.
    pub fn authors(&self) -> Vec<Author> {
        self.split()
            .into_iter()
            .filter(|(_, stat)| !stat.is_empty())
            .map(|(author, _)| author)
            .collect()
    }

    /// The paths carrying lines from more than one agent - the rows that get the `⚠` ring.
    pub fn shared_paths(&self) -> Vec<&Path> {
        self.entries
            .iter()
            .filter(|entry| entry.is_shared())
            .map(|entry| entry.path.as_path())
            .collect()
    }
}

/// Builds the change set for one worktree from a real diff and the recorded provenance.
///
/// `provenance` may be `None` (or simply know nothing about these paths), in which case every
/// line lands in [`Author::Unattributed`] and the result is exactly the file list and diffstat
/// the app already had - which is what makes this safe to put on the real path before any
/// attribution UI exists.
pub fn build_change_set(diff: &WorktreeDiff, provenance: Option<&WorktreeProvenance>) -> ChangeSet {
    // Keyed by path, so "one row per path" is a property of the data structure rather than a rule
    // the loop below has to remember.
    let mut rows: BTreeMap<PathBuf, ChangeSetEntry> = BTreeMap::new();
    let mut order: Vec<PathBuf> = Vec::new();

    for file in &diff.files {
        let split = split_for(file, provenance);
        match rows.get_mut(&file.path) {
            Some(existing) => {
                for (author, stat) in split {
                    *existing.split.entry(author).or_default() += stat;
                }
                existing.is_binary = existing.is_binary || file.is_binary;
            }
            None => {
                order.push(file.path.clone());
                rows.insert(
                    file.path.clone(),
                    ChangeSetEntry {
                        path: file.path.clone(),
                        status: file.status,
                        is_binary: file.is_binary,
                        split,
                    },
                );
            }
        }
    }

    ChangeSet {
        entries: order
            .into_iter()
            .filter_map(|path| rows.remove(&path))
            .collect(),
    }
}

/// Every diff line's author, hunk by hunk, index-aligned with `file.hunks[h].lines` - **the**
/// per-line answer, and the one the diff view's gutter bar paints (GitHub issue #287).
///
/// A **context** line is deliberately [`Author::Unattributed`]: it is a line this diff does not
/// change, so "who wrote it" is a question about history, not about this diff, and answering it
/// would paint an unchanged line as somebody's work. That is also why context lines contribute
/// nothing to [`split_for`]'s buckets - they are not part of the diffstat either.
///
/// [`split_for`] is **defined as** a fold over this, rather than a second walk of the same hunks
/// with the same rules. `STAGE-A-CHANGELOG.md` §5's open question was exactly that the run diff
/// and the split must not be able to drift; the gutter is a third reader of the same fact, and
/// the only way three readers cannot disagree is if there is one computation. So a line's tint
/// and its contribution to the row's `+n`/`−n` are the same decision, taken once.
pub fn line_authors(file: &DiffFile, provenance: Option<&WorktreeProvenance>) -> Vec<Vec<Author>> {
    let record = provenance.and_then(|records| records.get(&file.path));

    // The removal ledger, as remaining capacity per anchor. A removed diff line is attributed only
    // when the ledger really records a deletion at the position the diff says the line sat at -
    // both are positions in the *current* file, so they are directly comparable. A ledger entry
    // that lines up with nothing (it recorded a deletion of a line that was itself added after
    // this diff's base, so git never saw it) is simply never spent, and a removed line the ledger
    // cannot explain is `Unattributed`. Neither is padded out to make the numbers look complete.
    //
    // It is spent across the whole file in hunk order, which is why this is one pass over the
    // file rather than a per-hunk helper: two passes would each start with a full ledger and
    // spend the same recorded deletion twice.
    let mut ledger: BTreeMap<usize, Vec<(Author, u32)>> = BTreeMap::new();
    if let Some(record) = record {
        for mark in record.removals() {
            ledger
                .entry(mark.at)
                .or_default()
                .push((mark.author.clone(), mark.lines));
        }
    }

    let mut per_hunk: Vec<Vec<Author>> = Vec::with_capacity(file.hunks.len());
    for hunk in &file.hunks {
        // A header this app cannot parse means no line numbers, so nothing in it can be located -
        // its lines are counted honestly and attributed to nobody, rather than counted from a
        // guessed starting line.
        let new_start = parse_hunk_new_range(&hunk.header).map(|(start, _)| start);
        let mut new_line = new_start.unwrap_or(0);
        let mut authors: Vec<Author> = Vec::with_capacity(hunk.lines.len());

        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Context => {
                    new_line += 1;
                    authors.push(Author::Unattributed);
                }
                DiffLineKind::Added => {
                    authors.push(match (record, new_start) {
                        (Some(record), Some(_)) => record.author_at(new_line),
                        _ => Author::Unattributed,
                    });
                    new_line += 1;
                }
                DiffLineKind::Removed => {
                    // The removed line sat immediately before new line `new_line`, which is
                    // `new_line - 1` as the 0-based index `RemovalMark::at` uses.
                    authors.push(match new_start {
                        Some(_) => take_removal(&mut ledger, new_line.saturating_sub(1)),
                        None => Author::Unattributed,
                    });
                }
            }
        }
        per_hunk.push(authors);
    }

    per_hunk
}

/// Partitions one file's diff lines by author - a fold over [`line_authors`], never a second
/// walk of its own.
///
/// Every added and removed line lands in exactly one bucket, which is the whole reason the shares
/// sum to the total. Context lines are in neither, exactly as they are in neither half of a
/// diffstat.
fn split_for(
    file: &DiffFile,
    provenance: Option<&WorktreeProvenance>,
) -> BTreeMap<Author, DiffStat> {
    let mut split: BTreeMap<Author, DiffStat> = BTreeMap::new();
    for (hunk, authors) in file.hunks.iter().zip(line_authors(file, provenance)) {
        for (line, author) in hunk.lines.iter().zip(authors) {
            match line.kind {
                DiffLineKind::Context => {}
                DiffLineKind::Added => split.entry(author).or_default().added += 1,
                DiffLineKind::Removed => split.entry(author).or_default().removed += 1,
            }
        }
    }
    split
}

/// Spends one line of the ledger's recorded deletions at `anchor`, if it has any left.
fn take_removal(ledger: &mut BTreeMap<usize, Vec<(Author, u32)>>, anchor: usize) -> Author {
    let Some(marks) = ledger.get_mut(&anchor) else {
        return Author::Unattributed;
    };
    while let Some((author, remaining)) = marks.first_mut() {
        if *remaining == 0 {
            marks.remove(0);
            continue;
        }
        *remaining -= 1;
        return author.clone();
    }
    Author::Unattributed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use wt_core::diff::{diff_against_base, DiffHunk, DiffLine};

    use crate::provenance::store::ProvenanceStore;
    use crate::sidebar::changes::diff_file_stats;

    /// The base content of the design's shared file, as committed - its `Review · uncommitted`
    /// example (`src/api/users.rs`), reduced to the lines its hunks actually show.
    const USERS_RS_BASE: &str = "\
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

    /// After `s3` rewrote the `list` body.
    const USERS_RS_AFTER_S3: &str = "\
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

    /// After `s10` then rewrote the `search` body, in the same checkout.
    const USERS_RS_AFTER_S10: &str = "\
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
    const USERS_RS_AFTER_YOU: &str = "\
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

    struct Fixture {
        dir: tempfile::TempDir,
        store: ProvenanceStore,
        s3: AgentKey,
        s10: AgentKey,
    }

    impl Fixture {
        /// The mock's shared-file sad path, built for real: a git repo with a committed base, two
        /// agents that really edit through the store's own `PreToolUse`/write/`PostToolUse`
        /// sequence, and one real hand edit on top.
        fn two_agent_worktree() -> Fixture {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = dir.path();
            git(repo, &["init", "-b", "main"]);
            git(repo, &["config", "user.email", "test@example.com"]);
            git(repo, &["config", "user.name", "Test User"]);
            std::fs::create_dir_all(repo.join("src/api")).expect("mkdir");
            std::fs::write(repo.join("src/api/users.rs"), USERS_RS_BASE).expect("seed users");
            std::fs::write(repo.join("src/api/search.rs"), "fn search() {}\n")
                .expect("seed search");
            git(repo, &["add", "-A"]);
            git(repo, &["commit", "-m", "initial"]);

            let mut store = ProvenanceStore::default();
            let s3 = AgentKey::new("utf8:/repo/wt-a|Claude|1700000000");
            let s10 = AgentKey::new("utf8:/repo/wt-a|Claude|1700000900");

            agent_writes(&mut store, repo, "src/api/users.rs", &s3, USERS_RS_AFTER_S3);
            agent_writes(
                &mut store,
                repo,
                "src/api/users.rs",
                &s10,
                USERS_RS_AFTER_S10,
            );
            agent_writes(
                &mut store,
                repo,
                "src/api/search.rs",
                &s10,
                "fn search() {}\nfn search_cached() {}\n",
            );
            // The human's own edit, through the same door `AdeApp::record_hand_edit` uses.
            std::fs::write(repo.join("src/api/users.rs"), USERS_RS_AFTER_YOU).expect("hand edit");
            store.record_hand_edit(repo, &repo.join("src/api/users.rs"));

            Fixture {
                dir,
                store,
                s3,
                s10,
            }
        }

        fn change_set(&self) -> ChangeSet {
            let base = diff_against_base(self.dir.path()).expect("diff");
            let diff = base.diff().expect("a real diff");
            build_change_set(diff, self.store.worktree(self.dir.path()))
        }
    }

    #[test]
    fn a_path_three_authors_touched_is_one_row_carrying_all_three_and_a_combined_diffstat() {
        // `REVISION-2026-08-14.md` §1, rule 1: "A path appears once per worktree, however many
        // agents touched it - `by: ['s3','s10']` with a combined diffstat, never two rows."
        let fixture = Fixture::two_agent_worktree();
        let change_set = fixture.change_set();

        let shared = Path::new("src/api/users.rs");
        assert_eq!(
            change_set
                .entries()
                .iter()
                .filter(|entry| entry.path == shared)
                .count(),
            1,
            "two rows would give one path two staging checkboxes and suppress the shared-file ring"
        );

        let entry = change_set
            .entry(shared)
            .expect("the shared path must be a row");
        assert_eq!(
            entry.authors(),
            vec![
                Author::Agent(fixture.s3.clone()),
                Author::Agent(fixture.s10.clone()),
                Author::You,
            ],
            "both agents *and* the human's own hand edit, de-duplicated, on the one row"
        );
        assert!(
            entry.is_shared(),
            "more than one agent wrote here - this row gets the ⚠ ring"
        );

        // And the union is a fact about the *lines*, not a bookkeeping artifact of the row: three
        // different lines of this one file really do have three different authors, which is what
        // the gutter (GitHub issue #287) will read straight off the same store.
        let records = fixture
            .store
            .worktree(fixture.dir.path())
            .expect("the worktree must be tracked");
        assert_eq!(
            records.author_at(shared, 3),
            Author::Agent(fixture.s3.clone()),
            "the `list` body line is s3's"
        );
        assert_eq!(
            records.author_at(shared, 7),
            Author::Agent(fixture.s10.clone()),
            "the first `search` body line is s10's"
        );
        assert_eq!(
            records.author_at(shared, 11),
            Author::You,
            "and the `// TODO: cache key must include tenant_id` line is the human's own"
        );
        assert_eq!(
            records.author_at(shared, 1),
            Author::Unattributed,
            "while the `impl UserApi` opening line - which nobody touched - stays nobody's"
        );

        // The combined diffstat is the real one git reports for this path.
        assert_eq!(
            (entry.stat().added, entry.stat().removed),
            {
                let base = diff_against_base(fixture.dir.path()).expect("diff");
                let file = base
                    .diff()
                    .expect("diff")
                    .files
                    .iter()
                    .find(|file| file.path == shared)
                    .expect("changed");
                diff_file_stats(file)
            },
            "the row's combined diffstat must be git's own, not a number the store invented"
        );
    }

    #[test]
    fn every_share_of_a_shared_path_sums_to_its_combined_diffstat() {
        // `STAGE-A-CHANGELOG.md` §5's open question, answered by construction: "the run diff and
        // the split are the same fact counted two ways".
        let fixture = Fixture::two_agent_worktree();
        let change_set = fixture.change_set();

        for entry in change_set.entries() {
            let summed = entry
                .split()
                .values()
                .copied()
                .fold(DiffStat::default(), DiffStat::plus);
            assert_eq!(
                summed,
                entry.stat(),
                "{}'s shares must add up to its combined diffstat",
                entry.path.display()
            );
        }

        let summed = change_set
            .split()
            .values()
            .copied()
            .fold(DiffStat::default(), DiffStat::plus);
        assert_eq!(
            summed,
            change_set.total(),
            "the per-run diffstats must sum to the uncommitted union"
        );
    }

    #[test]
    fn each_author_is_credited_with_exactly_the_lines_they_really_wrote() {
        let fixture = Fixture::two_agent_worktree();
        let change_set = fixture.change_set();
        let entry = change_set
            .entry(Path::new("src/api/users.rs"))
            .expect("row");

        assert_eq!(
            entry.share(&Author::Agent(fixture.s3.clone())),
            DiffStat::new(1, 1),
            "s3 replaced one line of the `list` body"
        );
        assert_eq!(
            entry.share(&Author::Agent(fixture.s10.clone())),
            DiffStat::new(3, 1),
            "s10 replaced one line of `search` with three"
        );
        assert_eq!(
            entry.share(&Author::You),
            DiffStat::new(1, 0),
            "the hand edit added exactly one line and removed none"
        );
        assert_eq!(
            entry.share(&Author::Unattributed),
            DiffStat::default(),
            "every line of this diff is accounted for, so nothing falls through"
        );
    }

    /// GitHub issue #287's gutter, at the level the gutter actually reads: one author per diff
    /// line. The mock's own acceptance criterion for `users.rs` is "**three distinct gutter
    /// tints, one of which is the neutral hand-edit tint**", which is only reachable if this
    /// really returns three different authors over the file's own lines.
    #[test]
    fn every_diff_line_of_the_shared_file_carries_the_author_who_really_wrote_it() {
        let fixture = Fixture::two_agent_worktree();
        let base = diff_against_base(fixture.dir.path()).expect("diff");
        let diff = base.diff().expect("a real diff");
        let file = diff
            .files
            .iter()
            .find(|file| file.path == Path::new("src/api/users.rs"))
            .expect("the shared file must be in the diff");

        let authors = line_authors(file, fixture.store.worktree(fixture.dir.path()));
        assert_eq!(
            authors.len(),
            file.hunks.len(),
            "one entry per hunk, so a caller can index this with the same (hunk, line) pair it \
             already indexes the hunks with"
        );
        for (hunk, hunk_authors) in file.hunks.iter().zip(&authors) {
            assert_eq!(
                hunk.lines.len(),
                hunk_authors.len(),
                "and one entry per line inside it - the gutter reads this positionally"
            );
        }

        // Who wrote what, by the line's own text rather than by index, so this test states the
        // real claim rather than a coordinate.
        let author_of = |needle: &str| -> Author {
            for (hunk, hunk_authors) in file.hunks.iter().zip(&authors) {
                for (line, author) in hunk.lines.iter().zip(hunk_authors) {
                    if line.kind == DiffLineKind::Added && line.content.contains(needle) {
                        return author.clone();
                    }
                }
            }
            panic!("no added line containing {needle:?}");
        };
        assert_eq!(
            author_of("QueryBuilder::table"),
            Author::Agent(fixture.s3.clone()),
            "the `list` rewrite is s3's line"
        );
        assert_eq!(
            author_of("cache.get_or_load"),
            Author::Agent(fixture.s10.clone()),
            "the `search` rewrite is s10's"
        );
        assert_eq!(
            author_of("cache key must include tenant_id"),
            Author::You,
            "and the human's own hand edit flipped that line back to `you` (Orca's second rule)"
        );

        // Context lines are nobody's. The design gives its unchanged rows no author at all, and
        // painting one would tint a line this diff does not change.
        for (hunk, hunk_authors) in file.hunks.iter().zip(&authors) {
            for (line, author) in hunk.lines.iter().zip(hunk_authors) {
                if line.kind == DiffLineKind::Context {
                    assert_eq!(
                        *author,
                        Author::Unattributed,
                        "context line {:?} must carry no attribution",
                        line.content
                    );
                }
            }
        }
    }

    /// The gutter and the row's `+n`/`−n` are one computation, not two that agree today -
    /// `STAGE-A-CHANGELOG.md` §5's own open question, closed structurally.
    #[test]
    fn the_per_author_split_is_exactly_the_fold_of_the_per_line_authors() {
        let fixture = Fixture::two_agent_worktree();
        let base = diff_against_base(fixture.dir.path()).expect("diff");
        let diff = base.diff().expect("a real diff");
        let records = fixture.store.worktree(fixture.dir.path());
        let change_set = fixture.change_set();

        for file in &diff.files {
            let mut folded: BTreeMap<Author, DiffStat> = BTreeMap::new();
            for (hunk, hunk_authors) in file.hunks.iter().zip(line_authors(file, records)) {
                for (line, author) in hunk.lines.iter().zip(hunk_authors) {
                    match line.kind {
                        DiffLineKind::Context => {}
                        DiffLineKind::Added => folded.entry(author).or_default().added += 1,
                        DiffLineKind::Removed => folded.entry(author).or_default().removed += 1,
                    }
                }
            }
            let entry = change_set.entry(&file.path).expect("row");
            assert_eq!(
                &folded,
                entry.split(),
                "{}'s gutter and its row's diffstat must be the same partition",
                file.path.display()
            );
        }
    }

    /// The `by` union for a whole worktree - the graph's working-tree row (audit item I4).
    #[test]
    fn the_worktree_wide_author_union_is_everyone_who_wrote_anything_in_it() {
        let fixture = Fixture::two_agent_worktree();
        let change_set = fixture.change_set();
        assert_eq!(
            change_set.authors(),
            vec![
                Author::Agent(fixture.s3.clone()),
                Author::Agent(fixture.s10.clone()),
                Author::You,
            ],
            "both agents and the hand edit - `s: 's3'` pinning one agent to the whole working \
             tree is exactly what I4 removed"
        );
    }

    #[test]
    fn agent_file_counts_deliberately_do_not_sum_to_the_worktree_file_count() {
        // `REVISION-2026-07-31.md` §4's honest arithmetic: a shared path is one row for the
        // worktree and one file for *each* agent in it. Summing them is the bug, not the total.
        let fixture = Fixture::two_agent_worktree();
        let change_set = fixture.change_set();

        assert_eq!(change_set.len(), 2, "two changed paths in this worktree");
        assert_eq!(
            change_set.file_count_for(&Author::Agent(fixture.s3.clone())),
            1
        );
        assert_eq!(
            change_set.file_count_for(&Author::Agent(fixture.s10.clone())),
            2
        );
        assert_eq!(
            change_set.file_count_for(&Author::Agent(fixture.s3.clone()))
                + change_set.file_count_for(&Author::Agent(fixture.s10.clone())),
            3,
            "1 + 2 = 3 against a worktree count of 2 - that is what 'they are both working on \
             this file' means, and it must not be reconciled away"
        );
        assert_eq!(
            change_set.shared_paths(),
            vec![Path::new("src/api/users.rs")],
            "only the path with lines from more than one agent gets the ring"
        );
        assert!(
            !change_set
                .entry(Path::new("src/api/search.rs"))
                .expect("row")
                .is_shared(),
            "one agent's own file is not shared"
        );
    }

    #[test]
    fn a_worktree_with_no_recorded_provenance_reports_the_same_diffstat_it_always_did() {
        // The property that makes it safe to put this on the app's real Changes rows before any
        // attribution exists: with nothing recorded, every line lands in the unattributed bucket
        // and the row's number is exactly `diff_file_stats`'.
        let fixture = Fixture::two_agent_worktree();
        let base = diff_against_base(fixture.dir.path()).expect("diff");
        let diff = base.diff().expect("diff");
        let change_set = build_change_set(diff, None);

        assert_eq!(change_set.len(), diff.files.len());
        for file in &diff.files {
            let entry = change_set.entry(&file.path).expect("row");
            let (add, del) = diff_file_stats(file);
            assert_eq!((entry.stat().added, entry.stat().removed), (add, del));
            assert_eq!(
                entry.authors(),
                if add + del == 0 {
                    Vec::new()
                } else {
                    vec![Author::Unattributed]
                },
                "nothing is claimed, and nothing is guessed"
            );
            assert!(!entry.is_shared());
        }
    }

    #[test]
    fn a_diff_that_lists_one_path_twice_is_merged_into_one_row_rather_than_appended() {
        // "One row per path" is a property of the data structure here, not a rule the caller has
        // to remember - so it must hold even for an input that breaks it.
        let file = |added: &str| DiffFile {
            path: PathBuf::from("src/api/users.rs"),
            old_path: None,
            status: FileChangeStatus::Modified,
            is_binary: false,
            hunks: vec![DiffHunk {
                header: "@@ -1,1 +1,2 @@".to_string(),
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Context,
                        content: "impl UserApi {".to_string(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Added,
                        content: added.to_string(),
                    },
                ],
            }],
            truncated: false,
        };
        let diff = WorktreeDiff {
            base_branch: "main".to_string(),
            base_commit: "0".repeat(40),
            files: vec![file("one"), file("two")],
            truncated: false,
        };

        let change_set = build_change_set(&diff, None);
        assert_eq!(change_set.len(), 1);
        assert_eq!(
            change_set
                .entry(Path::new("src/api/users.rs"))
                .expect("row")
                .stat(),
            DiffStat::new(2, 0),
            "the two rows' lines belong to one row's combined diffstat"
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

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
