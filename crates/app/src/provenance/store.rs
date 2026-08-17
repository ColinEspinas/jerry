//! The provenance store itself: who wrote each line, kept per worktree and per path
//! (GitHub issue #284).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use imara_diff::intern::{InternedInput, TokenSource};
use imara_diff::{diff, Algorithm};

use super::{AgentKey, Author, DiffStat};

/// Largest file the store will track. Beyond this a path is dropped to "no recorded author"
/// rather than tracked at a cost nobody asked for: the store keeps the file's content in memory
/// (it is the "what was there before" half of every future diff), so this is a real per-tracked-
/// file memory bound, not a parse guard.
pub const MAX_TRACKED_BYTES: usize = 2 * 1024 * 1024;

/// Largest file, in lines, the store will track. A separate bound from [`MAX_TRACKED_BYTES`]
/// because the per-line vectors are what actually scale with this number: a 2 MiB minified bundle
/// is one line and costs nothing to track, where a 2 MiB file of `x\n` is a million of them.
pub const MAX_TRACKED_LINES: usize = 200_000;

/// A run of lines deleted from a path, and who deleted them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalMark {
    pub at: usize,
    pub author: Author,
    pub lines: u32,
}

/// What a call to [`WorktreeProvenance::record`] actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The content changed, and the changed lines are now this author's.
    Attributed,
    /// This path had no recorded state, so there was nothing to diff against. The content is now
    /// the baseline, and every line of it is [`Author::Unattributed`] - the honest answer, since
    /// nothing here knows which of those lines this author actually wrote.
    Seeded,
    /// The content is byte-identical to what was already recorded - a `PostToolUse` for a tool
    /// that read rather than wrote, or a save of an unmodified buffer. Nothing is attributed,
    /// because nothing changed.
    Unchanged,
    /// The file is gone, unreadable, not valid UTF-8, or past [`MAX_TRACKED_BYTES`] /
    /// [`MAX_TRACKED_LINES`]. Any recorded state for it is dropped rather than left describing
    /// content that no longer exists.
    Untracked,
}

/// The recorded authorship of one path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PathProvenance {
    /// One entry per line of [`Self::content`], in order.
    authors: Vec<Author>,
    /// The content the authors describe - the "before" half of the next diff. Held in memory
    /// only; see [`super::persist_state`] for what crosses a restart and what does not.
    content: String,
    /// The deletion ledger, sorted by [`RemovalMark::at`].
    removals: Vec<RemovalMark>,
}

impl PathProvenance {
    /// Who wrote line `line` (**1-based**, matching every line number in a diff hunk header).
    /// [`Author::Unattributed`] for a line nobody is on record for, and for a line number past
    /// the end of the recorded content - an out-of-range question is the absence of an answer,
    /// never a panic and never a wrapped-around neighbour's author.
    pub fn author_at(&self, line: usize) -> Author {
        match line
            .checked_sub(1)
            .and_then(|index| self.authors.get(index))
        {
            Some(author) => author.clone(),
            None => Author::Unattributed,
        }
    }

    pub fn line_count(&self) -> usize {
        self.authors.len()
    }

    /// The deletion ledger, in file order.
    pub fn removals(&self) -> &[RemovalMark] {
        &self.removals
    }

    /// The content these authors describe, as last observed.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Every author on record for this path, de-duplicated, in [`Author`]'s own order (agents
    /// first, then `you`, then unattributed). Includes authors known only from the deletion
    /// ledger - an agent that deleted lines and added none still touched this path.
    pub fn authors(&self) -> Vec<Author> {
        let mut seen: Vec<Author> = self.authors.clone();
        seen.extend(self.removals.iter().map(|mark| mark.author.clone()));
        seen.sort();
        seen.dedup();
        seen
    }

    /// Replaces the recorded content wholesale, attributing every line that changed to `author`
    /// and recording every line that vanished in the ledger.
    fn apply(&mut self, new_content: &str, author: &Author) {
        let before: Vec<&str> = new_lines(&self.content);
        let after: Vec<&str> = new_lines(new_content);

        let input = InternedInput::new(LineTokens(&before), LineTokens(&after));
        let mut changes: Vec<(std::ops::Range<u32>, std::ops::Range<u32>)> = Vec::new();
        diff(
            Algorithm::Histogram,
            &input,
            |b: std::ops::Range<u32>, a: std::ops::Range<u32>| changes.push((b, a)),
        );

        let mut authors = Vec::with_capacity(after.len());
        let mut removals: Vec<RemovalMark> = Vec::new();
        // The ledger is sorted by `at`, and `Sink::process_change` guarantees strictly increasing
        // ranges, so both can be walked once, forwards, in lockstep.
        let mut ledger = self.removals.iter().peekable();
        let mut old_cursor = 0usize;

        for (before_range, after_range) in changes {
            let change_start = before_range.start as usize;
            let unchanged_new_start = authors.len();

            // Carry every mark anchored at or before the change: an unchanged run maps 1:1, so a
            // mark's new anchor is its old one shifted by however far the run has moved.
            while let Some(mark) = ledger.peek() {
                if mark.at > change_start {
                    break;
                }
                #[allow(clippy::expect_used)]
                let mark = ledger.next().expect("peeked");
                removals.push(RemovalMark {
                    at: unchanged_new_start + (mark.at - old_cursor),
                    author: mark.author.clone(),
                    lines: mark.lines,
                });
            }
            // A mark strictly inside the rewritten region described lines that are no longer
            // adjacent to anything it was anchored against, so it is dropped rather than moved
            // onto content it does not describe.
            while ledger
                .peek()
                .is_some_and(|mark| mark.at < before_range.end as usize)
            {
                ledger.next();
            }

            // The unchanged run keeps the authors it already had.
            for index in old_cursor..change_start {
                authors.push(self.authors[index].clone());
            }
            debug_assert_eq!(authors.len(), after_range.start as usize);

            if before_range.end > before_range.start {
                removals.push(RemovalMark {
                    at: after_range.start as usize,
                    author: author.clone(),
                    lines: before_range.end - before_range.start,
                });
            }
            for _ in after_range.start..after_range.end {
                authors.push(author.clone());
            }
            old_cursor = before_range.end as usize;
        }

        let unchanged_new_start = authors.len();
        for mark in ledger {
            removals.push(RemovalMark {
                at: unchanged_new_start + (mark.at - old_cursor),
                author: mark.author.clone(),
                lines: mark.lines,
            });
        }
        for index in old_cursor..before.len() {
            authors.push(self.authors[index].clone());
        }

        removals.sort_by_key(|mark| mark.at);
        merge_adjacent(&mut removals);

        self.authors = authors;
        self.content = new_content.to_owned();
        self.removals = removals;
    }

    /// Rebuilds this record from persisted spans - see [`super::persist_state`]. `content` must be
    /// the exact content the spans were computed against; the caller proves that with a digest.
    pub(crate) fn from_parts(
        content: String,
        authors: Vec<Author>,
        mut removals: Vec<RemovalMark>,
    ) -> PathProvenance {
        removals.retain(|mark| mark.at <= authors.len() && mark.lines > 0);
        removals.sort_by_key(|mark| mark.at);
        merge_adjacent(&mut removals);
        PathProvenance {
            authors,
            content,
            removals,
        }
    }

    pub(crate) fn author_spans(&self) -> &[Author] {
        &self.authors
    }
}

/// Two runs of deletions at the same spot by the same author are one run - keeping them apart
/// would make the ledger's length depend on how many separate tool calls happened to touch the
/// same place, which is not a fact about the file.
fn merge_adjacent(removals: &mut Vec<RemovalMark>) {
    let mut merged: Vec<RemovalMark> = Vec::with_capacity(removals.len());
    for mark in removals.drain(..) {
        match merged.last_mut() {
            Some(last) if last.at == mark.at && last.author == mark.author => {
                last.lines = last.lines.saturating_add(mark.lines);
            }
            _ => merged.push(mark),
        }
    }
    *removals = merged;
}

/// The provenance of every tracked path in one worktree. Paths are worktree-relative, matching
/// `wt_core::diff::DiffFile::path`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeProvenance {
    paths: BTreeMap<PathBuf, PathProvenance>,
}

impl WorktreeProvenance {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn get(&self, relative: &Path) -> Option<&PathProvenance> {
        self.paths.get(relative)
    }

    pub fn paths(&self) -> impl Iterator<Item = (&Path, &PathProvenance)> {
        self.paths
            .iter()
            .map(|(path, record)| (path.as_path(), record))
    }

    /// Who wrote `relative`'s line `line` (1-based). [`Author::Unattributed`] for an untracked
    /// path, which is the same answer as "a line nobody is on record for" - and deliberately so:
    /// the UI must not be able to tell "we are not watching this file" apart from "we are, and
    /// nobody claims this line", because both mean *do not draw a tint*.
    pub fn author_at(&self, relative: &Path, line: usize) -> Author {
        match self.paths.get(relative) {
            Some(record) => record.author_at(line),
            None => Author::Unattributed,
        }
    }

    /// Snapshots `content` as the baseline for `relative` if there is not already one, attributing
    /// nothing. Called from an agent's `PreToolUse`, before it writes.
    pub fn begin_edit(&mut self, relative: &Path, content: &str) -> bool {
        if self.paths.contains_key(relative) {
            return false;
        }
        self.paths
            .insert(relative.to_path_buf(), seeded(content.to_owned()));
        true
    }

    /// Records `content` as `relative`'s new state, attributing every changed line to `author`.
    pub fn record(&mut self, relative: &Path, content: &str, author: &Author) -> RecordOutcome {
        let Some(record) = self.paths.get_mut(relative) else {
            self.paths
                .insert(relative.to_path_buf(), seeded(content.to_owned()));
            return RecordOutcome::Seeded;
        };
        if record.content == content {
            return RecordOutcome::Unchanged;
        }
        record.apply(content, author);
        RecordOutcome::Attributed
    }

    /// Drops everything recorded for `relative` - the file is gone, unreadable, or too big to
    /// track. Its lines become unattributed, which is the honest answer once the store can no
    /// longer see what it was describing.
    pub fn forget(&mut self, relative: &Path) {
        self.paths.remove(relative);
    }

    pub(crate) fn insert_record(&mut self, relative: PathBuf, record: PathProvenance) {
        self.paths.insert(relative, record);
    }
}

fn seeded(content: String) -> PathProvenance {
    let authors = vec![Author::Unattributed; new_lines(&content).len()];
    PathProvenance {
        authors,
        content,
        removals: Vec::new(),
    }
}

/// Every worktree's provenance, and the real file reads that feed it.
#[derive(Debug, Clone, Default)]
pub struct ProvenanceStore {
    worktrees: BTreeMap<PathBuf, WorktreeProvenance>,
}

impl ProvenanceStore {
    pub fn is_empty(&self) -> bool {
        self.worktrees.values().all(WorktreeProvenance::is_empty)
    }

    pub fn worktree(&self, worktree: &Path) -> Option<&WorktreeProvenance> {
        self.worktrees.get(worktree)
    }

    pub fn worktrees(&self) -> impl Iterator<Item = (&Path, &WorktreeProvenance)> {
        self.worktrees
            .iter()
            .map(|(path, record)| (path.as_path(), record))
    }

    pub(crate) fn worktree_mut(&mut self, worktree: &Path) -> &mut WorktreeProvenance {
        self.worktrees.entry(worktree.to_path_buf()).or_default()
    }

    /// Takes the "before" snapshot for a tool call an agent is about to make, reading the file as
    /// it stands right now. Safe and cheap to call for a path that is already tracked (it does
    /// nothing), and for a tool call that turns out not to write anything (the snapshot is simply
    /// never diffed against).
    pub fn begin_agent_edit(&mut self, worktree: &Path, file: &Path) {
        let Some(relative) = relative_within(worktree, file) else {
            return;
        };
        let before = snapshot_for_edit(&worktree.join(&relative));
        self.begin_agent_edit_with(worktree, file, before);
    }

    /// [`Self::begin_agent_edit`] for a caller that already read the file at the right moment.
    /// `None` means the file was unreadable or untrackable then, so there is no baseline to take -
    /// the path simply stays unattributed rather than getting a wrong one.
    pub fn begin_agent_edit_with(&mut self, worktree: &Path, file: &Path, before: Option<String>) {
        let (Some(relative), Some(before)) = (relative_within(worktree, file), before) else {
            return;
        };
        self.worktree_mut(worktree).begin_edit(&relative, &before);
    }

    /// Records the result of an agent's tool call: reads the file as it now stands and attributes
    /// everything that changed since [`Self::begin_agent_edit`] to `agent`.
    pub fn record_agent_edit(
        &mut self,
        worktree: &Path,
        file: &Path,
        agent: &AgentKey,
    ) -> RecordOutcome {
        self.record_from_disk(worktree, file, &Author::Agent(agent.clone()))
    }

    /// Records a hand edit - the human's own change, from Jerry's editor or from anything else
    /// that moved the bytes without a live agent's edit event behind it. Flips exactly the changed
    /// lines to [`Author::You`].
    pub fn record_hand_edit(&mut self, worktree: &Path, file: &Path) -> RecordOutcome {
        self.record_from_disk(worktree, file, &Author::You)
    }

    /// [`Self::record_hand_edit`] for a caller that already holds the content it just wrote -
    /// Jerry's own editor, which has the buffer in hand and should not race its own save by
    /// reading the file back.
    pub fn record_hand_edit_content(
        &mut self,
        worktree: &Path,
        file: &Path,
        content: &str,
    ) -> RecordOutcome {
        let Some(relative) = relative_within(worktree, file) else {
            return RecordOutcome::Untracked;
        };
        if !trackable(content) {
            self.worktree_mut(worktree).forget(&relative);
            return RecordOutcome::Untracked;
        }
        self.worktree_mut(worktree)
            .record(&relative, content, &Author::You)
    }

    fn record_from_disk(&mut self, worktree: &Path, file: &Path, author: &Author) -> RecordOutcome {
        let Some(relative) = relative_within(worktree, file) else {
            return RecordOutcome::Untracked;
        };
        match read_trackable(&worktree.join(&relative)) {
            Readable::Content(content) => self
                .worktree_mut(worktree)
                .record(&relative, &content, author),
            // A tracked file that has since been deleted really did lose every line it had, and
            // the author of the change that removed it is the one being recorded now - so this is
            // a real, ledgered removal, not a drop.
            Readable::Missing if self.tracks(worktree, &relative) => {
                self.worktree_mut(worktree).record(&relative, "", author)
            }
            Readable::Missing | Readable::Untrackable => {
                self.worktree_mut(worktree).forget(&relative);
                RecordOutcome::Untracked
            }
        }
    }

    fn tracks(&self, worktree: &Path, relative: &Path) -> bool {
        self.worktrees
            .get(worktree)
            .is_some_and(|records| records.get(relative).is_some())
    }

    /// Drops a whole worktree's attribution - for a worktree that has been removed.
    pub fn forget_worktree(&mut self, worktree: &Path) {
        self.worktrees.remove(worktree);
    }
}

/// The total each author is on record for across one path, from the store alone - the raw
/// material `super::change_set` re-derives against a real diff. Exposed because a caller that
/// wants "what has this agent done to this file since Jerry started watching", with no git
/// involved, should not have to reach into the line vector itself.
pub fn recorded_split(record: &PathProvenance) -> BTreeMap<Author, DiffStat> {
    let mut split: BTreeMap<Author, DiffStat> = BTreeMap::new();
    for author in record.author_spans() {
        if *author == Author::Unattributed {
            continue;
        }
        split.entry(author.clone()).or_default().added += 1;
    }
    for mark in record.removals() {
        split.entry(mark.author.clone()).or_default().removed += mark.lines;
    }
    split
}

/// Reads a file as the "before" half of an agent's edit, at the moment the edit is announced.
pub fn snapshot_for_edit(absolute: &Path) -> Option<String> {
    match read_trackable(absolute) {
        Readable::Content(content) => Some(content),
        Readable::Missing => Some(String::new()),
        Readable::Untrackable => None,
    }
}

/// The lines of `content`, exactly as [`PathProvenance`] counts them: `str::lines`, so a trailing
/// newline does not invent an empty final line and `\r\n` is handled the same way `imara_diff`'s
/// own line tokenizer handles it.
fn new_lines(content: &str) -> Vec<&str> {
    content.lines().collect()
}

/// A `TokenSource` over an already-split line vector.
struct LineTokens<'a>(&'a [&'a str]);

impl<'a> TokenSource for LineTokens<'a> {
    type Token = &'a str;
    type Tokenizer = std::iter::Copied<std::slice::Iter<'a, &'a str>>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().copied()
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len().try_into().unwrap_or(u32::MAX)
    }
}

enum Readable {
    Content(String),
    Missing,
    Untrackable,
}

fn read_trackable(absolute: &Path) -> Readable {
    let bytes = match std::fs::read(absolute) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Readable::Missing,
        Err(_) => return Readable::Untrackable,
    };
    if bytes.len() > MAX_TRACKED_BYTES {
        return Readable::Untrackable;
    }
    match String::from_utf8(bytes) {
        Ok(content) if trackable(&content) => Readable::Content(content),
        Ok(_) | Err(_) => Readable::Untrackable,
    }
}

fn trackable(content: &str) -> bool {
    content.len() <= MAX_TRACKED_BYTES && content.lines().count() <= MAX_TRACKED_LINES
}

/// `file` expressed relative to `worktree`, or `None` if it is not inside it.
pub fn relative_within(worktree: &Path, file: &Path) -> Option<PathBuf> {
    let relative = if file.is_absolute() {
        file.strip_prefix(worktree).ok()?
    } else {
        file
    };
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if normalized.as_os_str().is_empty() {
        return None;
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use test_support::{git, git_output};

    /// The real shape of one agent tool call, end to end: the `PreToolUse` snapshot, the agent's
    /// actual write to the actual file, then the `PostToolUse` record. No step is simulated - the
    /// bytes really land on disk in between, which is the only thing the store ever reads.
    fn agent_writes(
        store: &mut ProvenanceStore,
        worktree: &Path,
        relative: &str,
        agent: &AgentKey,
        content: &str,
    ) -> RecordOutcome {
        let file = worktree.join(relative);
        store.begin_agent_edit(worktree, &file);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&file, content).expect("write");
        store.record_agent_edit(worktree, &file, agent)
    }

    /// The human's own save, through the same door Jerry's editor uses.
    fn you_write(
        store: &mut ProvenanceStore,
        worktree: &Path,
        relative: &str,
        content: &str,
    ) -> RecordOutcome {
        let file = worktree.join(relative);
        std::fs::write(&file, content).expect("write");
        store.record_hand_edit(worktree, &file)
    }

    fn authors_of(store: &ProvenanceStore, worktree: &Path, relative: &str) -> Vec<Author> {
        store
            .worktree(worktree)
            .and_then(|records| records.get(Path::new(relative)))
            .map(|record| record.author_spans().to_vec())
            .expect("the path must be tracked")
    }

    fn agent(name: &str) -> AgentKey {
        AgentKey::new(name)
    }

    const FIVE_LINES: &str = "one\ntwo\nthree\nfour\nfive\n";

    #[test]
    fn an_agents_edit_claims_only_the_lines_it_actually_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        std::fs::write(dir.path().join("a.txt"), FIVE_LINES).expect("seed");

        let s3 = agent("s3");
        assert_eq!(
            agent_writes(
                &mut store,
                dir.path(),
                "a.txt",
                &s3,
                "one\nTWO\nthree\nFOUR\nfive\n"
            ),
            RecordOutcome::Attributed
        );

        assert_eq!(
            authors_of(&store, dir.path(), "a.txt"),
            vec![
                Author::Unattributed,
                Author::Agent(s3.clone()),
                Author::Unattributed,
                Author::Agent(s3),
                Author::Unattributed,
            ],
            "the three untouched lines must stay unattributed - an agent that rewrote two lines \
             did not write the file"
        );
    }

    #[test]
    fn a_hand_edit_flips_exactly_that_line_back_to_you_and_nothing_else() {
        // The first hard-won rule, and the one this whole model is built on: your own hand
        // edit flips that line back to you.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        std::fs::write(dir.path().join("a.txt"), FIVE_LINES).expect("seed");

        let s3 = agent("s3");
        agent_writes(
            &mut store,
            dir.path(),
            "a.txt",
            &s3,
            "one\nTWO\nthree\nFOUR\nfive\n",
        );
        assert_eq!(
            you_write(
                &mut store,
                dir.path(),
                "a.txt",
                "one\nTWO\nthree\nFOUR BY HAND\nfive\n"
            ),
            RecordOutcome::Attributed
        );

        assert_eq!(
            authors_of(&store, dir.path(), "a.txt"),
            vec![
                Author::Unattributed,
                Author::Agent(s3),
                Author::Unattributed,
                Author::You,
                Author::Unattributed,
            ],
            "a hand edit must flip exactly the line it changed - not the file, and not the other \
             lines the same agent wrote"
        );
    }

    #[test]
    fn a_second_agent_editing_the_same_file_does_not_inherit_the_first_agents_lines() {
        // The failure this guards against is the one that makes per-agent attribution worthless:
        // the second agent to touch a shared file appearing to have written all of it.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        std::fs::write(dir.path().join("a.txt"), FIVE_LINES).expect("seed");

        let s3 = agent("s3");
        let s10 = agent("s10");
        agent_writes(
            &mut store,
            dir.path(),
            "a.txt",
            &s3,
            "one\nTWO\nthree\nfour\nfive\n",
        );
        agent_writes(
            &mut store,
            dir.path(),
            "a.txt",
            &s10,
            "one\nTWO\nthree\nFOUR\nfive\n",
        );

        assert_eq!(
            authors_of(&store, dir.path(), "a.txt"),
            vec![
                Author::Unattributed,
                Author::Agent(s3),
                Author::Unattributed,
                Author::Agent(s10),
                Author::Unattributed,
            ]
        );
    }

    #[test]
    fn an_insertion_carries_every_surviving_lines_author_to_its_new_position() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        std::fs::write(dir.path().join("a.txt"), FIVE_LINES).expect("seed");

        let s3 = agent("s3");
        let s10 = agent("s10");
        agent_writes(
            &mut store,
            dir.path(),
            "a.txt",
            &s3,
            "one\ntwo\nthree\nFOUR\nfive\n",
        );
        // Two lines inserted at the very top: everything below shifts down by two, and the line
        // `s3` wrote must move with it rather than staying at index 3.
        agent_writes(
            &mut store,
            dir.path(),
            "a.txt",
            &s10,
            "zero\nhalf\none\ntwo\nthree\nFOUR\nfive\n",
        );

        let authors = authors_of(&store, dir.path(), "a.txt");
        assert_eq!(authors.len(), 7);
        assert_eq!(authors[0], Author::Agent(s10.clone()));
        assert_eq!(authors[1], Author::Agent(s10));
        assert_eq!(
            authors[2..5],
            [
                Author::Unattributed,
                Author::Unattributed,
                Author::Unattributed
            ]
        );
        assert_eq!(
            authors[5],
            Author::Agent(s3),
            "the line s3 wrote moved from index 3 to index 5, and must have taken its author \
             with it"
        );
        assert_eq!(authors[6], Author::Unattributed);
    }

    #[test]
    fn a_deletion_is_recorded_in_the_ledger_because_a_deleted_line_has_no_line_to_hang_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        std::fs::write(dir.path().join("a.txt"), FIVE_LINES).expect("seed");

        let s3 = agent("s3");
        agent_writes(&mut store, dir.path(), "a.txt", &s3, "one\nfour\nfive\n");

        let record = store
            .worktree(dir.path())
            .and_then(|records| records.get(Path::new("a.txt")))
            .expect("tracked");
        assert_eq!(
            record.removals(),
            &[RemovalMark {
                at: 1,
                author: Author::Agent(s3.clone()),
                lines: 2,
            }],
            "two lines vanished from just after line one, and s3 is who removed them"
        );
        assert_eq!(
            recorded_split(record).get(&Author::Agent(s3)),
            Some(&DiffStat::new(0, 2)),
            "a pure deletion is a real −2 for that agent even though it added no line"
        );
    }

    #[test]
    fn recording_without_a_before_snapshot_attributes_nothing_rather_than_the_whole_file() {
        // What a Jerry launched mid-tool-call sees: the `PostToolUse` arrives with no matching
        // `PreToolUse`, so there is genuinely nothing to diff against. Claiming the file for that
        // agent would be the single most damaging guess this store could make.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, FIVE_LINES).expect("seed");

        assert_eq!(
            store.record_agent_edit(dir.path(), &file, &agent("s3")),
            RecordOutcome::Seeded
        );
        assert_eq!(
            authors_of(&store, dir.path(), "a.txt"),
            vec![Author::Unattributed; 5]
        );
    }

    #[test]
    fn a_before_snapshot_never_erases_what_is_already_recorded() {
        // `begin_edit` fires on every tool call, including the second and third to the same file.
        // Re-seeding on those would wipe every author recorded so far.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, FIVE_LINES).expect("seed");

        let s3 = agent("s3");
        agent_writes(
            &mut store,
            dir.path(),
            "a.txt",
            &s3,
            "one\nTWO\nthree\nfour\nfive\n",
        );
        store.begin_agent_edit(dir.path(), &file);

        assert_eq!(
            authors_of(&store, dir.path(), "a.txt")[1],
            Author::Agent(s3)
        );
    }

    #[test]
    fn an_unchanged_file_is_reported_as_such_rather_than_reattributed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        std::fs::write(dir.path().join("a.txt"), FIVE_LINES).expect("seed");

        let s3 = agent("s3");
        agent_writes(
            &mut store,
            dir.path(),
            "a.txt",
            &s3,
            "one\nTWO\nthree\nfour\nfive\n",
        );
        assert_eq!(
            store.record_agent_edit(dir.path(), &dir.path().join("a.txt"), &agent("s10")),
            RecordOutcome::Unchanged
        );
        assert_eq!(
            authors_of(&store, dir.path(), "a.txt")[1],
            Author::Agent(s3),
            "an agent that changed nothing must take nothing"
        );
    }

    #[test]
    fn a_binary_or_oversized_file_is_dropped_rather_than_tracked_with_wrong_line_numbers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        let file = dir.path().join("blob.bin");
        std::fs::write(&file, FIVE_LINES).expect("seed");
        let s3 = agent("s3");
        agent_writes(
            &mut store,
            dir.path(),
            "blob.bin",
            &s3,
            "one\nTWO\nthree\nfour\nfive\n",
        );
        assert!(store
            .worktree(dir.path())
            .expect("tracked")
            .get(Path::new("blob.bin"))
            .is_some());

        std::fs::write(&file, [0u8, 159, 146, 150]).expect("write binary");
        assert_eq!(
            store.record_agent_edit(dir.path(), &file, &s3),
            RecordOutcome::Untracked
        );
        assert_eq!(
            store
                .worktree(dir.path())
                .expect("worktree")
                .author_at(Path::new("blob.bin"), 1),
            Author::Unattributed,
            "a file the store can no longer read line by line reads as unattributed, not as \
             whatever it last said"
        );
    }

    #[test]
    fn a_deleted_file_keeps_its_removal_in_the_ledger_rather_than_vanishing_silently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, FIVE_LINES).expect("seed");
        let s3 = agent("s3");
        agent_writes(
            &mut store,
            dir.path(),
            "a.txt",
            &s3,
            "one\nTWO\nthree\nfour\nfive\n",
        );

        std::fs::remove_file(&file).expect("remove");
        assert_eq!(
            store.record_agent_edit(dir.path(), &file, &s3),
            RecordOutcome::Attributed
        );

        let record = store
            .worktree(dir.path())
            .and_then(|records| records.get(Path::new("a.txt")))
            .expect("tracked");
        assert_eq!(record.line_count(), 0);
        assert_eq!(
            record.removals(),
            &[RemovalMark {
                at: 0,
                author: Author::Agent(s3),
                lines: 5,
            }]
        );
    }

    #[test]
    fn a_line_number_outside_the_file_is_unattributed_rather_than_a_panic_or_a_neighbour() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ProvenanceStore::default();
        std::fs::write(dir.path().join("a.txt"), FIVE_LINES).expect("seed");
        agent_writes(
            &mut store,
            dir.path(),
            "a.txt",
            &agent("s3"),
            "one\nTWO\nthree\nfour\nfive\n",
        );

        let records = store.worktree(dir.path()).expect("worktree");
        assert_eq!(
            records.author_at(Path::new("a.txt"), 0),
            Author::Unattributed
        );
        assert_eq!(
            records.author_at(Path::new("a.txt"), 900),
            Author::Unattributed
        );
        assert_eq!(
            records.author_at(Path::new("never-seen.txt"), 1),
            Author::Unattributed
        );
    }

    #[test]
    fn a_path_outside_the_worktree_is_refused_rather_than_recorded_under_it() {
        // A hook payload is untrusted input off a socket (see `crate::hooks::server`'s threat
        // model), and its `file_path` is a string the model chose.
        let worktree = Path::new("/repo/wt-a");
        assert_eq!(
            relative_within(worktree, Path::new("/repo/wt-a/src/main.rs")),
            Some(PathBuf::from("src/main.rs"))
        );
        assert_eq!(
            relative_within(worktree, Path::new("src/main.rs")),
            Some(PathBuf::from("src/main.rs"))
        );
        assert_eq!(
            relative_within(worktree, Path::new("./src/main.rs")),
            Some(PathBuf::from("src/main.rs"))
        );
        for outside in [
            "/etc/passwd",
            "/repo/wt-b/src/main.rs",
            "../../etc/passwd",
            "src/../../etc/passwd",
        ] {
            assert_eq!(
                relative_within(worktree, Path::new(outside)),
                None,
                "{outside} is not inside {}",
                worktree.display()
            );
        }
    }

    #[test]
    fn nothing_a_recording_store_does_ever_touches_the_worktree_or_a_commit_made_from_it() {
        // The second hard-won rule: attribution is local, never committed. That is true by
        // construction; this is what makes the claim checkable rather than a promise.
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        git(repo, &["init", "-b", "main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test User"]);
        std::fs::write(repo.join("a.txt"), FIVE_LINES).expect("seed");
        git(repo, &["add", "a.txt"]);
        git(repo, &["commit", "-m", "initial"]);

        let mut store = ProvenanceStore::default();
        let s3 = AgentKey::new("utf8:/repo/wt-a|Claude|1700000000");
        agent_writes(
            &mut store,
            repo,
            "a.txt",
            &s3,
            "one\nTWO\nthree\nfour\nfive\n",
        );
        you_write(
            &mut store,
            repo,
            "a.txt",
            "one\nTWO\nthree\nBY HAND\nfive\n",
        );

        assert_eq!(authors_of(&store, repo, "a.txt")[3], Author::You);

        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-m", "the agent's work"]);

        let marker = s3.as_str();
        for args in [
            vec!["show", "--format=raw", "HEAD"],
            vec!["log", "--format=full", "--all"],
            vec!["cat-file", "-p", "HEAD"],
        ] {
            let output = git_output(repo, &args);
            assert!(
                !output.contains(marker) && !output.to_lowercase().contains("provenance"),
                "`git {}` leaked attribution:\n{output}",
                args.join(" ")
            );
        }
        assert_eq!(
            git_output(repo, &["notes", "list"]),
            "",
            "attribution must not be smuggled out as a git note either"
        );
        assert_eq!(
            git_output(repo, &["for-each-ref", "--format=%(refname)"]),
            "refs/heads/main",
            "the store must not anchor anything with a ref of its own"
        );
        assert_eq!(
            git_output(repo, &["status", "--porcelain"]),
            "",
            "the store must not leave a single file behind in the worktree"
        );
    }
}
