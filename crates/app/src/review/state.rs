//! Pure, GPUI-window-free state and wording for the agent review surface - see `super`'s module
//! docs for the scope this belongs to.
//!
//! Everything here is directly unit-testable without a live window: what a baseline *is*, what
//! an agent's review currently holds, the persisted key an agent resolves to, and the exact
//! words the Review tab's header says. Actually capturing a baseline, loading a diff, and
//! drawing any of it happens in the sibling `flow`/`render` modules.

use std::path::{Path, PathBuf};

use wt_core::diff::WorktreeDiff;
use wt_core::review::UntrackedCoverage;

use crate::root::plural;
use crate::work_surface::agents::AgentKind;

/// Why a baseline is where it is - the two, and only two, real ways a baseline is ever set
/// (GitHub issue #225, phase 1's decided scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineReason {
    /// Captured when the agent was spawned. The review diff therefore reads "everything this
    /// agent has done since it started".
    Spawn,
    /// Captured when the user clicked `Mark reviewed`. The review diff reads "everything that
    /// has changed since you last looked".
    MarkedReviewed,
}

impl BaselineReason {
    /// The "since ..." phrase the Review tab's header uses for this reason. Vocabulary is
    /// deliberately review-side, never git-side: this surface says "since", "review",
    /// "unreviewed", "mark reviewed" - never a bare "diff", which is the git side's word and
    /// whose overloading is precisely what GitHub issue #225 reports as confusing.
    pub fn since_phrase(self) -> &'static str {
        match self {
            BaselineReason::Spawn => "since it started",
            BaselineReason::MarkedReviewed => "since you last reviewed",
        }
    }

    /// The stable string this reason persists as. Written by hand rather than derived, so
    /// renaming a variant can never silently invalidate every already-written state file.
    pub fn as_key(self) -> &'static str {
        match self {
            BaselineReason::Spawn => "spawn",
            BaselineReason::MarkedReviewed => "marked-reviewed",
        }
    }

    /// The inverse of [`Self::as_key`]. `None` for an unrecognised value (a hand-edited or
    /// future-written state file), which callers treat as "this entry is unusable" rather than
    /// guessing a reason and then telling the user something false about their own review.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "spawn" => Some(BaselineReason::Spawn),
            "marked-reviewed" => Some(BaselineReason::MarkedReviewed),
            _ => None,
        }
    }
}

/// One captured review baseline: the real git tree an agent's changes are measured against, the
/// ref keeping that tree alive, and when and why it was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewBaseline {
    /// The real hex tree id from `wt_core::review::snapshot_worktree_tree`.
    pub tree_id: String,
    /// The `refs/jerry/review/*` ref anchoring [`Self::tree_id`] against `git gc` -
    /// `wt_core::review::baseline_ref_name`'s output for this agent's own key.
    pub ref_name: String,
    /// When this snapshot was really taken, as seconds since the Unix epoch.
    pub taken_at_unix: i64,
    pub reason: BaselineReason,
    /// Whether this snapshot actually captured the worktree's untracked files, or fell back to
    /// tracked paths only because the untracked set was too large to hash
    /// (`wt_core::review::MAX_UNTRACKED_SNAPSHOT_BYTES`).
    pub untracked: UntrackedCoverage,
}

/// The Review tab's background diff load - mirrors `crate::graph_view::state::GraphLoadState`'s
/// three-real-outcomes-plus-not-started shape, for one consistent idiom across this crate's
/// background-loaded surfaces.
#[derive(Debug, Clone, Default)]
pub enum ReviewLoadState {
    /// No load has been started yet (the tab has never been opened, or the baseline just
    /// advanced) - deliberately not eager, since `wt_core::review::diff_against_tree` spawns a
    /// real `git diff` process.
    #[default]
    NotLoaded,
    Loading,
    /// A real, parsed review diff. An empty `files` list here is the good, correct empty state -
    /// "nothing new since you last looked" - never an error.
    Loaded(WorktreeDiff),
    /// A real error message from the underlying `wt_core` call, surfaced rather than swallowed
    /// into a fabricated empty diff (which would read as "nothing changed" - the single most
    /// misleading thing this surface could say).
    Error(String),
}

/// Everything this app knows about one agent's review: where its baseline is, what's currently
/// loaded against it, and which file the user has open in the Review tab.
#[derive(Debug, Clone)]
pub struct AgentReview {
    pub baseline: ReviewBaseline,
    pub load: ReviewLoadState,
    /// Which of the review diff's changed files is expanded in the tab's detail pane. `None`
    /// means the file list is showing with nothing selected. Cleared whenever the baseline
    /// advances, since the previously-open file is - by definition - no longer changed.
    pub open_file: Option<PathBuf>,
    /// The paths that differ from this agent's baseline right now - refreshed cheaply on the
    /// rail's own status-poll tick (`wt_core::review::changed_paths_against_tree`: one
    /// `git diff --name-only` process, no hunk parsing at all).
    pub unreviewed_paths: Option<Vec<PathBuf>>,
}

impl AgentReview {
    pub fn new(baseline: ReviewBaseline) -> Self {
        Self {
            baseline,
            load: ReviewLoadState::NotLoaded,
            open_file: None,
            unreviewed_paths: None,
        }
    }

    /// The real, loaded review diff, if one is loaded. `None` while loading, on error, and
    /// before the first load - never a fabricated empty diff standing in for "don't know yet",
    /// which callers would be unable to tell apart from a genuine "nothing changed".
    pub fn diff(&self) -> Option<&WorktreeDiff> {
        match &self.load {
            ReviewLoadState::Loaded(diff) => Some(diff),
            ReviewLoadState::NotLoaded | ReviewLoadState::Loading | ReviewLoadState::Error(_) => {
                None
            }
        }
    }

    /// How many files this agent has really changed since its baseline, or `None` if that has
    /// not been measured yet. Reads [`Self::unreviewed_paths`] - see that field's docs for why
    /// the cheap measurement, not the loaded diff, is the source of truth here.
    pub fn unreviewed_file_count(&self) -> Option<usize> {
        self.unreviewed_paths.as_ref().map(Vec::len)
    }

    /// `true` only when a real measurement has landed **and** it found at least one changed
    /// file. An unmeasured review is not "has unreviewed changes": claiming so would put a
    /// `Review ready` status on the rail off the back of an answer git never actually gave.
    pub fn has_unreviewed_changes(&self) -> bool {
        self.unreviewed_file_count().is_some_and(|count| count > 0)
    }

    /// Advances this review onto a freshly captured baseline (`Mark reviewed`). The loaded diff
    /// and the open file are both dropped rather than kept: they describe the *old* baseline, and
    /// showing them next to a new one would be actively wrong. `flow` re-loads immediately
    /// afterwards, which - against a snapshot taken moments ago - lands on a real, empty diff.
    pub fn advance_to(&mut self, baseline: ReviewBaseline) {
        self.baseline = baseline;
        self.load = ReviewLoadState::NotLoaded;
        self.open_file = None;
        // Reset to "not measured yet" rather than to an empty list: the new baseline genuinely
        // has not been measured against yet, and asserting "nothing changed" before any git call
        // has run would be a fabricated answer that happens to usually be right.
        self.unreviewed_paths = None;
    }
}

/// The durable identity a persisted baseline is filed under: this agent's worktree, its kind,
/// and the wall-clock second it was spawned in.
pub fn baseline_key(worktree: &Path, kind: AgentKind, spawned_at_unix: i64) -> String {
    format!(
        "{}|{}|{spawned_at_unix}",
        encode_worktree(worktree),
        kind.label()
    )
}

/// A lossless, always-UTF-8 encoding of a worktree path, for use inside a [`baseline_key`] and in
/// the persisted state file.
pub fn encode_worktree(worktree: &Path) -> String {
    match worktree.to_str() {
        Some(text) => format!("utf8:{text}"),
        None => {
            let bytes = worktree.as_os_str().as_encoded_bytes();
            let mut encoded = String::with_capacity(6 + bytes.len() * 2);
            encoded.push_str("bytes:");
            for byte in bytes {
                encoded.push_str(&format!("{byte:02x}"));
            }
            encoded
        }
    }
}

/// The inverse of [`encode_worktree`] - `None` for anything this app didn't write (a hand-edited
/// or truncated state file), which callers treat as "this record isn't usable" rather than
/// guessing at a path.
pub fn decode_worktree(encoded: &str) -> Option<PathBuf> {
    if let Some(text) = encoded.strip_prefix("utf8:") {
        return Some(PathBuf::from(text));
    }
    let hex = encoded.strip_prefix("bytes:")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks(2) {
        let text = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(text, 16).ok()?);
    }
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// The Review tab's header text: what this review is measured against, and when that measurement
/// was taken - e.g. `claude \u{b7} since it started \u{b7} 09:31`.
pub fn review_tab_header(agent_label: &str, baseline: &ReviewBaseline) -> String {
    let mut header = format!(
        "{agent_label} \u{b7} {} \u{b7} {}",
        baseline.reason.since_phrase(),
        crate::rail::state::format_utc_hhmm(baseline.taken_at_unix),
    );
    // A tracked-only baseline answers a genuinely narrower question, so the header says so rather
    // than presenting it as the full answer. Same honesty rule as the rest of this header: state
    // what is really being measured, never imply more.
    if baseline.untracked == UntrackedCoverage::Excluded {
        header.push_str(" \u{b7} tracked files only");
    }
    header
}

/// The Review tab's own empty state, worded as the genuinely good outcome it is.
pub fn review_empty_message(reason: BaselineReason) -> &'static str {
    match reason {
        BaselineReason::Spawn => "nothing changed since this agent started",
        BaselineReason::MarkedReviewed => "nothing new since you last looked",
    }
}

/// The Review tab's strip label: how many files are unreviewed, or the honest loading/error
/// state. Never says "diff".
pub fn review_summary_label(review: &AgentReview) -> String {
    match &review.load {
        ReviewLoadState::NotLoaded | ReviewLoadState::Loading => "checking\u{2026}".to_string(),
        ReviewLoadState::Error(_) => "review unavailable".to_string(),
        ReviewLoadState::Loaded(diff) => match diff.files.len() {
            0 => "nothing unreviewed".to_string(),
            count => format!("{} unreviewed", plural::count(count, "file", None)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline(reason: BaselineReason, taken_at_unix: i64) -> ReviewBaseline {
        ReviewBaseline {
            tree_id: "a".repeat(40),
            ref_name: "refs/jerry/review/6b6579".to_string(),
            taken_at_unix,
            reason,
            untracked: UntrackedCoverage::Included,
        }
    }

    #[test]
    fn the_header_states_what_it_is_measured_against_and_when() {
        let header = review_tab_header(
            "claude",
            &baseline(BaselineReason::Spawn, 9 * 3600 + 31 * 60),
        );
        assert_eq!(header, "claude \u{b7} since it started \u{b7} 09:31");
    }

    #[test]
    fn a_marked_reviewed_baseline_says_since_you_last_reviewed_not_since_it_started() {
        let header = review_tab_header(
            "codex",
            &baseline(BaselineReason::MarkedReviewed, 12 * 3600 + 4 * 60),
        );
        assert_eq!(header, "codex \u{b7} since you last reviewed \u{b7} 12:04");
    }

    #[test]
    fn no_review_wording_anywhere_says_a_bare_diff() {
        let mut wording: Vec<String> = vec![
            review_tab_header("claude", &baseline(BaselineReason::Spawn, 0)),
            review_tab_header("claude", &baseline(BaselineReason::MarkedReviewed, 0)),
            review_empty_message(BaselineReason::Spawn).to_string(),
            review_empty_message(BaselineReason::MarkedReviewed).to_string(),
        ];
        for reason in [BaselineReason::Spawn, BaselineReason::MarkedReviewed] {
            wording.push(reason.since_phrase().to_string());
        }
        let mut review = AgentReview::new(baseline(BaselineReason::Spawn, 0));
        wording.push(review_summary_label(&review));
        review.load = ReviewLoadState::Error("boom".to_string());
        wording.push(review_summary_label(&review));

        for text in wording {
            assert!(
                !text.to_lowercase().contains("diff"),
                "the review surface must never say a bare 'diff' - got {text:?}"
            );
        }
    }

    #[test]
    fn the_two_empty_states_are_worded_differently_from_each_other() {
        assert_ne!(
            review_empty_message(BaselineReason::Spawn),
            review_empty_message(BaselineReason::MarkedReviewed)
        );
    }

    #[test]
    fn only_a_real_measurement_counts_as_having_unreviewed_changes() {
        let mut review = AgentReview::new(baseline(BaselineReason::Spawn, 0));
        assert!(
            !review.has_unreviewed_changes(),
            "nothing measured yet must never read as 'has changes'"
        );
        assert_eq!(
            review.unreviewed_file_count(),
            None,
            "and unmeasured must be honestly absent, not a fabricated 0"
        );

        review.unreviewed_paths = Some(Vec::new());
        assert!(
            !review.has_unreviewed_changes(),
            "measured as empty is a real answer, and it is 'nothing unreviewed'"
        );
        assert_eq!(
            review.unreviewed_file_count(),
            Some(0),
            "measured-empty and unmeasured must be distinguishable"
        );

        review.unreviewed_paths = Some(vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
        assert!(review.has_unreviewed_changes());
        assert_eq!(review.unreviewed_file_count(), Some(2));
    }

    #[test]
    fn no_load_state_short_of_loaded_ever_yields_a_diff() {
        let mut review = AgentReview::new(baseline(BaselineReason::Spawn, 0));
        assert!(review.diff().is_none(), "NotLoaded");
        assert_eq!(review_summary_label(&review), "checking\u{2026}");
        review.load = ReviewLoadState::Loading;
        assert!(review.diff().is_none(), "Loading");
        review.load = ReviewLoadState::Error("git exploded".to_string());
        assert!(review.diff().is_none(), "Error");
        assert_eq!(review_summary_label(&review), "review unavailable");

        review.load = ReviewLoadState::Loaded(WorktreeDiff {
            base_branch: "since it started".to_string(),
            base_commit: "a".repeat(40),
            files: Vec::new(),
            truncated: false,
        });
        assert!(review.diff().is_some());
        assert_eq!(review_summary_label(&review), "nothing unreviewed");
    }

    #[test]
    fn review_summary_label_conjugates_at_zero_one_and_two() {
        let label_for = |file_count: usize| {
            let mut review = AgentReview::new(baseline(BaselineReason::Spawn, 0));
            review.load = ReviewLoadState::Loaded(WorktreeDiff {
                base_branch: "since it started".to_string(),
                base_commit: "a".repeat(40),
                files: (0..file_count)
                    .map(|i| wt_core::diff::DiffFile {
                        path: PathBuf::from(format!("f{i}.rs")),
                        old_path: None,
                        status: wt_core::diff::FileChangeStatus::Modified,
                        is_binary: false,
                        hunks: Vec::new(),
                        truncated: false,
                    })
                    .collect(),
                truncated: false,
            });
            review_summary_label(&review)
        };
        assert_eq!(label_for(0), "nothing unreviewed");
        assert_eq!(label_for(1), "1 file unreviewed");
        assert_eq!(label_for(2), "2 files unreviewed");
        assert_eq!(label_for(7), "7 files unreviewed");
    }

    #[test]
    fn marking_reviewed_drops_the_old_loaded_review_and_the_open_file() {
        let mut review = AgentReview::new(baseline(BaselineReason::Spawn, 100));
        review.load = ReviewLoadState::Loaded(WorktreeDiff {
            base_branch: "since it started".to_string(),
            base_commit: "a".repeat(40),
            files: Vec::new(),
            truncated: false,
        });
        review.open_file = Some(PathBuf::from("src/main.rs"));

        let advanced = ReviewBaseline {
            tree_id: "b".repeat(40),
            reason: BaselineReason::MarkedReviewed,
            taken_at_unix: 200,
            ..baseline(BaselineReason::Spawn, 200)
        };
        review.advance_to(advanced.clone());

        assert_eq!(review.baseline, advanced);
        assert!(review.open_file.is_none());
        assert!(matches!(review.load, ReviewLoadState::NotLoaded));
        assert!(!review.has_unreviewed_changes());
    }

    #[test]
    fn every_baseline_reason_round_trips_through_its_persisted_key() {
        for reason in [BaselineReason::Spawn, BaselineReason::MarkedReviewed] {
            assert_eq!(BaselineReason::from_key(reason.as_key()), Some(reason));
        }
        assert_eq!(BaselineReason::from_key("pty-went-quiet"), None);
        assert_eq!(BaselineReason::from_key(""), None);
    }

    #[test]
    fn baseline_keys_differ_by_worktree_kind_and_spawn_second() {
        let base = baseline_key(Path::new("/repo/wt-a"), AgentKind::Claude, 1_700_000_000);
        assert_ne!(
            base,
            baseline_key(Path::new("/repo/wt-b"), AgentKind::Claude, 1_700_000_000)
        );
        assert_ne!(
            base,
            baseline_key(Path::new("/repo/wt-a"), AgentKind::Codex, 1_700_000_000)
        );
        assert_ne!(
            base,
            baseline_key(Path::new("/repo/wt-a"), AgentKind::Claude, 1_700_000_001)
        );
        assert_eq!(
            base,
            baseline_key(Path::new("/repo/wt-a"), AgentKind::Claude, 1_700_000_000),
            "the same agent must resolve to the same key every time - that's what makes a \
             persisted baseline findable at all"
        );
    }

    #[test]
    fn a_worktree_path_round_trips_through_its_key_encoding() {
        for path in [
            "/repo/wt-a",
            "/home/u/My Repo/.worktrees/feature-x",
            "/tmp/utf8:looks-like-a-tag",
            "/tmp/bytes:6162",
        ] {
            let encoded = encode_worktree(Path::new(path));
            assert_eq!(
                decode_worktree(&encoded),
                Some(PathBuf::from(path)),
                "{path} must round-trip exactly - including paths that look like the encoding's \
                 own tags, which is why both arms carry an explicit prefix"
            );
        }
    }

    #[test]
    fn a_key_encoding_this_app_did_not_write_is_refused_rather_than_guessed() {
        assert_eq!(decode_worktree("/repo/wt-a"), None, "no tag at all");
        assert_eq!(decode_worktree("bytes:abc"), None, "odd-length hex");
        assert_eq!(decode_worktree("bytes:zz"), None, "not hex");
    }

    #[cfg(unix)]
    #[test]
    fn worktrees_differing_only_in_invalid_utf8_get_distinct_keys() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let a = PathBuf::from(OsStr::from_bytes(b"/repo/\xff"));
        let b = PathBuf::from(OsStr::from_bytes(b"/repo/\xfe"));
        assert_ne!(
            a, b,
            "precondition: these really are different paths on this platform"
        );
        assert_eq!(
            a.display().to_string(),
            b.display().to_string(),
            "precondition: and `display()` really does flatten them to the same string - which is \
             exactly the bug this encoding replaces"
        );

        assert_ne!(
            baseline_key(&a, AgentKind::Claude, 1),
            baseline_key(&b, AgentKind::Claude, 1),
            "two genuinely different worktrees must never share one baseline key"
        );
    }

    #[test]
    fn a_tracked_only_baseline_says_so_in_the_header() {
        let mut tracked_only = baseline(BaselineReason::Spawn, 9 * 3600 + 31 * 60);
        tracked_only.untracked = UntrackedCoverage::Excluded;
        assert_eq!(
            review_tab_header("claude", &tracked_only),
            "claude \u{b7} since it started \u{b7} 09:31 \u{b7} tracked files only"
        );

        assert_eq!(
            review_tab_header(
                "claude",
                &baseline(BaselineReason::Spawn, 9 * 3600 + 31 * 60)
            ),
            "claude \u{b7} since it started \u{b7} 09:31"
        );
    }

    #[test]
    fn two_identical_agents_spawned_in_the_same_second_share_a_key_a_known_limitation() {
        assert_eq!(
            baseline_key(Path::new("/repo/wt-a"), AgentKind::Claude, 1_700_000_000),
            baseline_key(Path::new("/repo/wt-a"), AgentKind::Claude, 1_700_000_000),
        );
    }
}
