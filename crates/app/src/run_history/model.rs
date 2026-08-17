//! The pure, GPUI-free half of agent history (GitHub issue #227): outcomes, drift bands, the
//! repo → worktree → run tree, every word this surface says, and the synthesised transcript.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::hooks::history::{PastAgent, RunDiffstat};
use crate::rail::status::Status;
use crate::root::plural;
use crate::theme;
use crate::work_surface::agents::AgentKind;

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Its last turn ended cleanly, and Jerry watched the run end.
    Done,
    /// It ended while it was still working, or still waiting on a human.
    Interrupted,
    /// Its last real signal was a failure.
    Failed,
    /// Nobody ever saw it end - the record simply stopped being updated.
    Abandoned,
}

impl Outcome {
    /// The real outcome of a real record.
    pub fn of(run: &PastAgent) -> Outcome {
        if run.ended_at_unix.is_none() {
            return Outcome::Abandoned;
        }
        match run.status {
            Status::Fail => Outcome::Failed,
            Status::Run | Status::Ask => Outcome::Interrupted,
            Status::Review | Status::Idle => Outcome::Done,
        }
    }

    /// The pill's text - §5's own four words, lowercase as the design writes them.
    pub const fn label(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Interrupted => "interrupted",
            Outcome::Failed => "failed",
            Outcome::Abandoned => "abandoned",
        }
    }

    pub const fn fg(self) -> theme::ColorToken {
        match self {
            Outcome::Done => theme::history::OUT_DONE_FG,
            Outcome::Interrupted => theme::history::OUT_INTERRUPTED_FG,
            Outcome::Failed => theme::history::OUT_FAILED_FG,
            Outcome::Abandoned => theme::history::OUT_ABANDONED_FG,
        }
    }

    pub const fn bg(self) -> theme::ColorToken {
        match self {
            Outcome::Done => theme::history::OUT_DONE_BG,
            Outcome::Interrupted => theme::history::OUT_INTERRUPTED_BG,
            Outcome::Failed => theme::history::OUT_FAILED_BG,
            Outcome::Abandoned => theme::history::OUT_ABANDONED_BG,
        }
    }

    /// How the synthesised closing line opens, per outcome - the design's own four strings.
    pub const fn closing_lead(self) -> &'static str {
        match self {
            Outcome::Done => "Finished.",
            Outcome::Interrupted => "Interrupted by you.",
            Outcome::Failed => "Exited non-zero. Nothing further was attempted.",
            Outcome::Abandoned => "Left unfinished.",
        }
    }
}

/// How far the branch has moved since the run ended - three bands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftBand {
    /// 0 commits since - `at the tip`.
    Tip,
    /// 1-2 commits since.
    Near,
    /// 3+ commits since.
    Far,
}

impl DriftBand {
    pub const fn of(commits: usize) -> DriftBand {
        match commits {
            0 => DriftBand::Tip,
            1..=2 => DriftBand::Near,
            _ => DriftBand::Far,
        }
    }

    pub const fn dot(self) -> theme::ColorToken {
        match self {
            DriftBand::Tip => theme::history::DRIFT_TIP,
            DriftBand::Near => theme::history::DRIFT_NEAR,
            DriftBand::Far => theme::history::DRIFT_FAR,
        }
    }

    /// The drift *label*'s colour. §4's table names one only for the far band; the other two leave
    /// the label in the list's own recessive text, so colour on the words means "this one has
    /// moved a long way" rather than restating the dot.
    pub const fn text(self) -> theme::ColorToken {
        match self {
            DriftBand::Tip | DriftBand::Near => theme::history::DRIFT_TEXT,
            DriftBand::Far => theme::history::DRIFT_FAR_TEXT,
        }
    }
}

/// The short drift label a history row shows - §4's `at the tip` / `N commits since`.
pub fn drift_label(commits: usize) -> String {
    if commits == 0 {
        return "at the tip".to_string();
    }
    format!("{} since", plural::count(commits, "commit", None))
}

/// The spelled-out consequence sentence the transcript tab's footer carries - §4, verbatim in both
/// arms, with singular and plural on *both* the count and its verb.
pub fn drift_sentence(commits: usize) -> String {
    if commits == 0 {
        return "Nothing has landed since this run ended \u{2014} it resumes on the files it left."
            .to_string();
    }
    format!(
        "{} {} landed since. Resuming replays the transcript as context against the current files.",
        plural::count(commits, "commit", None),
        plural::form(commits, "has", "have"),
    )
}

/// Which runs the History view is showing - the `all` / `this worktree` toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryScope {
    #[default]
    All,
    ThisWorktree,
}

impl HistoryScope {
    /// Both scopes, in the order the toggle paints them.
    pub const ALL: &'static [HistoryScope] = &[HistoryScope::All, HistoryScope::ThisWorktree];

    pub const fn label(self) -> &'static str {
        match self {
            HistoryScope::All => "all",
            HistoryScope::ThisWorktree => "this worktree",
        }
    }

    /// The segment's tooltip - the mock's own `every worktree` / the active branch's name.
    pub fn hint(self, branch: Option<&str>) -> String {
        match self {
            HistoryScope::All => "every worktree".to_string(),
            HistoryScope::ThisWorktree => match branch {
                Some(branch) => branch.to_string(),
                None => "the selected worktree".to_string(),
            },
        }
    }
}

/// The sidebar's empty state - §3's *No agent has run in `<branch>` yet.*, and the mock's own
/// wider-scope wording for the case where the whole window has no history at all.
pub fn empty_note(scope: HistoryScope, branch: Option<&str>) -> String {
    match (scope, branch) {
        (HistoryScope::ThisWorktree, Some(branch)) => {
            format!("No agent has run in {branch} yet.")
        }
        (HistoryScope::ThisWorktree, None) => "No agent has run in this worktree yet.".to_string(),
        (HistoryScope::All, _) => "No agent has finished a run yet.".to_string(),
    }
}

/// The note shown when there really is history, but this view's filter text hides all of it -
/// the same shape [`crate::rail::strip::problems_filtered_away_note`] uses, for the same reason:
/// "nothing here" and "nothing *matching*" are different facts.
pub fn filtered_away_note(hidden: usize) -> String {
    format!("No match in the {}.", plural::count(hidden, "run", None))
}

// ---------------------------------------------------------------------- one run's own wording

/// A run's title - the first line: `agent chip · title · duration`.
pub fn run_title(run: &PastAgent) -> String {
    run.title
        .clone()
        .or_else(|| run.question.clone())
        .or_else(|| run.activity.clone())
        .unwrap_or_else(|| format!("{} session", run.kind.label()))
}

/// How long the run really lasted, from its own spawn moment to its own end - the row's trailing
/// `6m` and the transcript header's `24m`.
pub fn run_duration(run: &PastAgent) -> Option<String> {
    let ended = run.ended_at_unix?;
    let seconds = ended.saturating_sub(run.spawned_at_unix);
    (seconds > 0).then(|| format_run_duration(seconds))
}

/// A finished run's duration, in the design's own `6m` / `1h 02m` shape.
pub fn format_run_duration(seconds: i64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

/// When the run finished, in the design's own `today 09:41` / `yesterday 16:12` / `2 days ago`
/// shape.
pub fn run_when(finished_at_unix: i64, now_unix: i64) -> String {
    const DAY: i64 = 86_400;
    let days_ago = now_unix.div_euclid(DAY) - finished_at_unix.div_euclid(DAY);
    match days_ago {
        // A run stamped in the future is a clock that moved, not a run that has not happened.
        // Reading it as "today" is the least wrong of the available answers.
        i64::MIN..=0 => format!(
            "today {}",
            crate::rail::state::format_utc_hhmm(finished_at_unix)
        ),
        1 => format!(
            "yesterday {}",
            crate::rail::state::format_utc_hhmm(finished_at_unix)
        ),
        days => format!("{} ago", plural::count(days as usize, "day", None)),
    }
}

/// The moment a run's history row and transcript are dated by: its real ending where there is one,
/// and otherwise the last moment Jerry heard from it - which for an abandoned run is the only real
/// moment there is.
pub fn run_finished_at(run: &PastAgent) -> i64 {
    run.ended_at_unix.unwrap_or(run.updated_at_unix)
}

/// The transcript header's meta line - §3's
/// `<agent> · <when> · 24m · 21 turns · 6 files · +148 −96`.
pub fn run_meta_line(run: &PastAgent, now_unix: i64) -> String {
    let mut parts: Vec<String> = vec![
        run.kind.label().to_string(),
        run_when(run_finished_at(run), now_unix),
    ];
    if let Some(duration) = run_duration(run) {
        parts.push(duration);
    }
    if run.turns > 0 {
        parts.push(plural::count(run.turns as usize, "turn", None));
    }
    if let Some(diffstat) = run.diffstat {
        parts.push(plural::count(diffstat.files as usize, "file", None));
        parts.push(format!(
            "+{} \u{2212}{}",
            diffstat.insertions, diffstat.deletions
        ));
    }
    parts.join(" \u{b7} ")
}

/// The run-transcript tab's own label - §3's `sonnet-4.5 · today 09:41`, with this app's real
/// agent-kind label in place of the mock's model name (Jerry knows which CLI it spawned, and does
/// not know which model that CLI chose).
pub fn run_tab_label(run: &PastAgent, now_unix: i64) -> String {
    format!(
        "{} \u{b7} {}",
        run.kind.label(),
        run_when(run_finished_at(run), now_unix)
    )
}

/// Whether a run matches the History view's filter text, over exactly the strings the row shows:
/// its title, its branch (supplied by the caller, since a run record stores a path, not a branch)
/// and its agent kind. A blank query matches everything.
pub fn matches_filter(run: &PastAgent, branch: Option<&str>, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    run_title(run).to_lowercase().contains(&query)
        || run.kind.label().to_lowercase().contains(&query)
        || branch.is_some_and(|branch| branch.to_lowercase().contains(&query))
}

// ---------------------------------------------------------------- repo -> worktree -> run tree

/// One worktree, as input to [`build_run_tree`] - the caller's reduction of a real repo's
/// worktree list, so this module needs no notion of what a repo is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryWorktree {
    pub path: PathBuf,
    /// The repo this checkout belongs to, as the header should print it.
    pub repo_label: String,
    /// The branch name the group row shows; the path's own label where there is no branch.
    pub label: String,
    pub branch: Option<String>,
}

/// One run, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEntry {
    pub run: PastAgent,
    /// How many commits have landed in this run's checkout since it ended - `None` while the
    /// real `git` traversal behind it has not answered yet, or could not
    /// (`crate::run_history::flow`). A row with no answer paints no drift dot and no drift text
    /// rather than an invented `at the tip`.
    pub drift: Option<usize>,
}

impl RunEntry {
    pub fn outcome(&self) -> Outcome {
        Outcome::of(&self.run)
    }

    pub fn band(&self) -> Option<DriftBand> {
        self.drift.map(DriftBand::of)
    }
}

/// One worktree's runs, under a repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunGroup {
    pub worktree: PathBuf,
    pub label: String,
    pub branch: Option<String>,
    /// Whether this is the window's currently selected worktree - it carries the selection edge
    /// and opens by default.
    pub is_active: bool,
    pub open: bool,
    pub runs: Vec<RunEntry>,
}

/// One repo's groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRepo {
    pub label: String,
    pub groups: Vec<RunGroup>,
}

impl RunRepo {
    pub fn run_count(&self) -> usize {
        self.groups.iter().map(|group| group.runs.len()).sum()
    }
}

/// The whole tree the History body paints, plus the two counts it needs to choose between its
/// three states (rows, "nothing here", "nothing matching").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunTree {
    pub repos: Vec<RunRepo>,
    /// How many runs are in scope *before* the filter - what tells "this worktree has no history"
    /// apart from "your filter matched none of its history".
    pub unfiltered: usize,
}

impl RunTree {
    pub fn total(&self) -> usize {
        self.repos.iter().map(RunRepo::run_count).sum()
    }

    /// How many worktree groups the tree really paints, across every repo.
    pub fn worktree_count(&self) -> usize {
        self.repos.iter().map(|repo| repo.groups.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// The view's own count line - each view's body opens with one, and both list headers are
    /// **tallied over their own data**.
    pub fn count_line(&self) -> Option<String> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        Some(format!(
            "{} \u{b7} {}",
            plural::count(total, "run", None),
            plural::count(self.worktree_count(), "worktree", None),
        ))
    }
}

/// The rail's own `↺ 2 earlier runs` line under a worktree row.
pub fn earlier_runs_label(count: usize) -> String {
    format!("{count} earlier {}", plural::form(count, "run", "runs"))
}

/// Builds the real repo → worktree → run tree.
pub fn build_run_tree(
    runs: &[PastAgent],
    worktrees: &[HistoryWorktree],
    active_worktree: Option<&Path>,
    scope: HistoryScope,
    query: &str,
    collapsed: &HashMap<PathBuf, bool>,
    drift: &HashMap<PathBuf, HashMap<String, usize>>,
) -> RunTree {
    let mut by_worktree: HashMap<&Path, Vec<&PastAgent>> = HashMap::new();
    for run in runs {
        by_worktree
            .entry(run.worktree.as_path())
            .or_default()
            .push(run);
    }

    let mut tree = RunTree::default();
    for worktree in worktrees {
        if scope == HistoryScope::ThisWorktree && Some(worktree.path.as_path()) != active_worktree {
            continue;
        }
        let Some(all_runs) = by_worktree.get(worktree.path.as_path()) else {
            continue;
        };
        tree.unfiltered += all_runs.len();

        let worktree_drift = drift.get(&worktree.path);
        let runs: Vec<RunEntry> = all_runs
            .iter()
            .filter(|run| matches_filter(run, worktree.branch.as_deref(), query))
            .map(|run| RunEntry {
                run: (*run).clone(),
                drift: worktree_drift.and_then(|counts| counts.get(&run.key).copied()),
            })
            .collect();
        if runs.is_empty() {
            continue;
        }

        let is_active = Some(worktree.path.as_path()) == active_worktree;
        let open = match collapsed.get(&worktree.path) {
            Some(collapsed) => !collapsed,
            None => is_active || scope == HistoryScope::ThisWorktree,
        };
        let group = RunGroup {
            worktree: worktree.path.clone(),
            label: worktree
                .branch
                .clone()
                .unwrap_or_else(|| worktree.label.clone()),
            branch: worktree.branch.clone(),
            is_active,
            open,
            runs,
        };
        match tree
            .repos
            .iter_mut()
            .find(|repo| repo.label == worktree.repo_label)
        {
            Some(repo) => repo.groups.push(group),
            None => tree.repos.push(RunRepo {
                label: worktree.repo_label.clone(),
                groups: vec![group],
            }),
        }
    }
    tree
}

// -------------------------------------------------------------------------------- transcripts

/// How one transcript line is coloured. A **captured** transcript is plain text with no styling of
/// its own, so every one of its lines is [`LineTone::Body`]; the other three exist for the lines
/// this app writes itself - the synthesised transcript and the closing line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTone {
    /// The leading `❯ …` command line.
    Prompt,
    /// Ordinary output.
    Body,
    /// An indented `⎿ …` detail line.
    Detail,
    /// A `● …` lead line.
    Lead,
}

impl LineTone {
    pub const fn color(self) -> theme::ColorToken {
        match self {
            LineTone::Prompt => theme::history::TRANSCRIPT_PROMPT,
            LineTone::Body => theme::history::TRANSCRIPT_BODY,
            LineTone::Detail => theme::history::TRANSCRIPT_DETAIL,
            LineTone::Lead => theme::history::TRANSCRIPT_LEAD,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub text: String,
    pub tone: LineTone,
}

impl TranscriptLine {
    fn new(text: impl Into<String>, tone: LineTone) -> TranscriptLine {
        TranscriptLine {
            text: text.into(),
            tone,
        }
    }
}

/// The command a resume of this run would really be - the transcript's opening line when none was
/// captured. Only ever this run's own kind and its own checkout.
fn resume_command_line(run: &PastAgent, branch: Option<&str>) -> String {
    let command = match (run.kind, run.session_id.as_deref()) {
        (AgentKind::Claude, Some(session_id)) => format!("claude --resume {session_id}"),
        (AgentKind::Claude, None) => "claude".to_string(),
        // Neither of these resumes: `Agents::spawn_resume` is Claude-only, so printing a
        // `--resume` flag here would advertise something this app doesn't actually do for them.
        (AgentKind::Codex, _) => "codex".to_string(),
        (AgentKind::Cursor, _) => "cursor-agent".to_string(),
    };
    match branch {
        Some(branch) => format!("\u{276f} {command} \u{b7} {branch}"),
        None => format!("\u{276f} {command}"),
    }
}

/// The full body of a run-transcript tab: `captured` where a real transcript was stored for this
/// run, a short synthesis from the run's *own* record where none was, and in both cases the
/// synthesised closing line.
pub fn transcript_body(
    run: &PastAgent,
    branch: Option<&str>,
    captured: Option<&[String]>,
    now_unix: i64,
) -> Vec<TranscriptLine> {
    let mut lines: Vec<TranscriptLine> = match captured {
        Some(captured) if !captured.is_empty() => captured
            .iter()
            .map(|line| TranscriptLine::new(line.clone(), LineTone::Body))
            .collect(),
        _ => synthesised_body(run, branch),
    };
    lines.push(TranscriptLine::new(String::new(), LineTone::Body));
    lines.extend(closing_lines(run, now_unix));
    lines
}

/// The short stand-in §3 asks for where a run has no stored transcript: built "**from that run's
/// own record** (title, turns, file count); never fall back to another run's output".
fn synthesised_body(run: &PastAgent, branch: Option<&str>) -> Vec<TranscriptLine> {
    let mut lines = vec![
        TranscriptLine::new(resume_command_line(run, branch), LineTone::Prompt),
        TranscriptLine::new(String::new(), LineTone::Body),
        TranscriptLine::new(format!("\u{25cf} {}", run_title(run)), LineTone::Lead),
    ];
    if run.turns > 0 {
        let turns = plural::count(run.turns as usize, "turn", None);
        lines.push(TranscriptLine::new(
            match branch {
                Some(branch) => format!("  \u{2514} {turns} in {branch}"),
                None => format!("  \u{2514} {turns}"),
            },
            LineTone::Detail,
        ));
    }
    if let Some(diffstat) = run.diffstat {
        lines.push(TranscriptLine::new(
            format!(
                "  \u{2514} touched {}",
                plural::count(diffstat.files as usize, "file", None)
            ),
            LineTone::Detail,
        ));
    }
    if let Some(activity) = &run.activity {
        lines.push(TranscriptLine::new(
            format!("  \u{2514} last: {activity}"),
            LineTone::Detail,
        ));
    }
    lines.push(TranscriptLine::new(
        "  \u{2514} no transcript was captured for this run",
        LineTone::Detail,
    ));
    lines
}

/// The two closing lines every transcript ends on - §3's
/// `● Finished. 2 files changed, +41 −0.` and `⎿ run ended today 09:41 after 6m`.
pub fn closing_lines(run: &PastAgent, now_unix: i64) -> Vec<TranscriptLine> {
    let outcome = Outcome::of(run);
    let lead = match run.diffstat {
        Some(RunDiffstat {
            files,
            insertions,
            deletions,
        }) => format!(
            "\u{25cf} {} {} changed, +{insertions} \u{2212}{deletions}.",
            outcome.closing_lead(),
            plural::count(files as usize, "file", None),
        ),
        None => format!("\u{25cf} {}", outcome.closing_lead()),
    };

    let finished_at = run_finished_at(run);
    let when = run_when(finished_at, now_unix);
    let tail = match (run.ended_at_unix, run_duration(run)) {
        (Some(_), Some(duration)) => format!("  \u{2514} run ended {when} after {duration}"),
        (Some(_), None) => format!("  \u{2514} run ended {when}"),
        // No recorded ending at all: say what is actually known - when Jerry last heard from it -
        // rather than calling that moment the end of the run.
        (None, _) => format!("  \u{2514} last seen {when}"),
    };

    vec![
        TranscriptLine::new(lead, LineTone::Lead),
        TranscriptLine::new(tail, LineTone::Detail),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(worktree: &str, spawned: i64) -> PastAgent {
        PastAgent {
            key: format!("{worktree}|Claude|{spawned}"),
            worktree: PathBuf::from(worktree),
            kind: AgentKind::Claude,
            spawned_at_unix: spawned,
            status: Status::Idle,
            activity: None,
            question: None,
            updated_at_unix: spawned + 100,
            session_id: None,
            title: None,
            turns: 0,
            ended_at_unix: None,
            diffstat: None,
        }
    }

    fn worktree(path: &str, repo: &str, branch: &str) -> HistoryWorktree {
        HistoryWorktree {
            path: PathBuf::from(path),
            repo_label: repo.to_string(),
            label: branch.to_string(),
            branch: Some(branch.to_string()),
        }
    }

    // ------------------------------------------------------------------------------- outcomes

    #[test]
    fn a_run_nobody_watched_end_is_abandoned_whatever_its_last_status_said() {
        for status in [
            Status::Idle,
            Status::Review,
            Status::Run,
            Status::Ask,
            Status::Fail,
        ] {
            let mut past = run("/repo/wt", 1);
            past.status = status;
            past.ended_at_unix = None;
            assert_eq!(
                Outcome::of(&past),
                Outcome::Abandoned,
                "{status:?}: with no recorded ending, nobody saw this run finish"
            );
        }
    }

    #[test]
    fn a_watched_ending_reads_its_outcome_off_the_status_it_ended_on() {
        let cases = [
            (Status::Idle, Outcome::Done),
            (Status::Review, Outcome::Done),
            (Status::Run, Outcome::Interrupted),
            (Status::Ask, Outcome::Interrupted),
            (Status::Fail, Outcome::Failed),
        ];
        for (status, expected) in cases {
            let mut past = run("/repo/wt", 1);
            past.status = status;
            past.ended_at_unix = Some(500);
            assert_eq!(Outcome::of(&past), expected, "{status:?}");
        }
    }

    #[test]
    fn outcome_and_drift_are_independent_axes() {
        let mut past = run("/repo/wt", 1);
        past.ended_at_unix = None;
        let entry = RunEntry {
            run: past,
            drift: Some(0),
        };
        assert_eq!(entry.outcome(), Outcome::Abandoned);
        assert_eq!(entry.band(), Some(DriftBand::Tip));
    }

    #[test]
    fn there_is_no_merged_outcome() {
        for outcome in [
            Outcome::Done,
            Outcome::Interrupted,
            Outcome::Failed,
            Outcome::Abandoned,
        ] {
            assert_ne!(
                outcome.label(),
                "merged",
                "merging happens to a branch, not to a run"
            );
        }
    }

    // ---------------------------------------------------------------------------------- drift

    #[test]
    fn the_drift_bands_are_exactly_the_revisions_three() {
        assert_eq!(DriftBand::of(0), DriftBand::Tip);
        assert_eq!(DriftBand::of(1), DriftBand::Near);
        assert_eq!(DriftBand::of(2), DriftBand::Near);
        assert_eq!(DriftBand::of(3), DriftBand::Far);
        assert_eq!(DriftBand::of(97), DriftBand::Far);
    }

    #[test]
    fn drift_says_singular_and_plural_in_both_the_label_and_the_sentence() {
        assert_eq!(drift_label(0), "at the tip");
        assert_eq!(drift_label(1), "1 commit since");
        assert_eq!(drift_label(4), "4 commits since");

        assert_eq!(
            drift_sentence(0),
            "Nothing has landed since this run ended \u{2014} it resumes on the files it left."
        );
        assert!(
            drift_sentence(1).starts_with("1 commit has landed since."),
            "{}",
            drift_sentence(1)
        );
        assert!(
            drift_sentence(4).starts_with("4 commits have landed since."),
            "{}",
            drift_sentence(4)
        );
    }

    // -------------------------------------------------------------------------------- wording

    #[test]
    fn a_runs_title_is_its_own_prompt_and_falls_back_to_what_the_record_really_has() {
        let mut past = run("/repo/wt", 1);
        assert_eq!(
            run_title(&past),
            "Claude session",
            "a record with nothing in it claims nothing"
        );

        past.activity = Some("Edit: src/auth.rs".to_string());
        assert_eq!(run_title(&past), "Edit: src/auth.rs");

        past.question = Some("Bash needs permission".to_string());
        assert_eq!(run_title(&past), "Bash needs permission");

        past.title = Some("Reproduce the refresh race in a test".to_string());
        assert_eq!(run_title(&past), "Reproduce the refresh race in a test");
    }

    #[test]
    fn an_unfinished_run_has_no_duration_rather_than_a_made_up_one() {
        let mut past = run("/repo/wt", 1_700_000_000);
        past.updated_at_unix = 1_700_009_999;
        assert_eq!(
            run_duration(&past),
            None,
            "how long Jerry was listening is not how long the run lasted"
        );

        past.ended_at_unix = Some(1_700_000_360);
        assert_eq!(run_duration(&past).as_deref(), Some("6m"));
    }

    #[test]
    fn a_duration_keeps_its_minutes_past_the_hour() {
        assert_eq!(format_run_duration(45), "45s");
        assert_eq!(format_run_duration(60), "1m");
        assert_eq!(format_run_duration(24 * 60), "24m");
        assert_eq!(format_run_duration(62 * 60), "1h 02m");
        assert_eq!(format_run_duration(134 * 60), "2h 14m");
    }

    #[test]
    fn when_reads_today_yesterday_and_then_whole_days() {
        let day = 86_400;
        let at = 1_700_000_000;
        assert_eq!(run_when(at, at), "today 22:13");
        assert_eq!(run_when(at, at + day), "yesterday 22:13");
        assert_eq!(run_when(at, at + 2 * day), "2 days ago");
        assert_eq!(
            run_when(at, at + 3 * day),
            "3 days ago",
            "the count is pluralised through the one helper"
        );
    }

    #[test]
    fn a_run_stamped_in_the_future_reads_as_today_rather_than_a_negative_day_count() {
        let at = 1_700_000_000;
        assert!(run_when(at, at - 5_000).starts_with("today "));
    }

    #[test]
    fn the_meta_line_omits_what_was_never_measured_rather_than_printing_zeros() {
        let mut past = run("/repo/wt", 1_700_000_000);
        past.ended_at_unix = Some(1_700_000_000 + 24 * 60);
        assert_eq!(
            run_meta_line(&past, 1_700_000_000),
            "Claude \u{b7} today 22:37 \u{b7} 24m",
            "no turn count and no diffstat were measured, so neither is claimed"
        );

        past.turns = 21;
        past.diffstat = Some(RunDiffstat {
            files: 6,
            insertions: 148,
            deletions: 96,
        });
        assert_eq!(
            run_meta_line(&past, 1_700_000_000),
            "Claude \u{b7} today 22:37 \u{b7} 24m \u{b7} 21 turns \u{b7} 6 files \u{b7} +148 \u{2212}96"
        );
    }

    #[test]
    fn one_turn_and_one_file_are_singular() {
        let mut past = run("/repo/wt", 1_700_000_000);
        past.ended_at_unix = Some(1_700_000_060);
        past.turns = 1;
        past.diffstat = Some(RunDiffstat {
            files: 1,
            insertions: 9,
            deletions: 0,
        });
        let line = run_meta_line(&past, 1_700_000_000);
        assert!(line.contains("1 turn \u{b7}"), "{line}");
        assert!(line.contains("1 file \u{b7}"), "{line}");
    }

    // ----------------------------------------------------------------------------------- tree

    #[test]
    fn the_tree_follows_the_rails_own_repo_and_worktree_order() {
        let runs = vec![run("/a/wt-1", 10), run("/b/wt-2", 20), run("/a/wt-1", 30)];
        let worktrees = vec![
            worktree("/a/wt-1", "alpha", "feature-a"),
            worktree("/b/wt-2", "beta", "feature-b"),
        ];
        let tree = build_run_tree(
            &runs,
            &worktrees,
            Some(Path::new("/a/wt-1")),
            HistoryScope::All,
            "",
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(tree.repos.len(), 2);
        assert_eq!(tree.repos[0].label, "alpha");
        assert_eq!(tree.repos[0].run_count(), 2);
        assert_eq!(tree.repos[1].label, "beta");
        assert_eq!(tree.total(), 3);
    }

    #[test]
    fn a_run_whose_worktree_is_gone_is_dropped_rather_than_shown_unresumable() {
        let runs = vec![run("/gone/wt", 10)];
        let worktrees = vec![worktree("/a/wt-1", "alpha", "feature-a")];
        let tree = build_run_tree(
            &runs,
            &worktrees,
            None,
            HistoryScope::All,
            "",
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(tree.is_empty());
        assert_eq!(tree.unfiltered, 0);
    }

    #[test]
    fn the_this_worktree_scope_really_narrows_to_the_active_checkout() {
        let runs = vec![run("/a/wt-1", 10), run("/b/wt-2", 20)];
        let worktrees = vec![
            worktree("/a/wt-1", "alpha", "feature-a"),
            worktree("/b/wt-2", "beta", "feature-b"),
        ];
        let tree = build_run_tree(
            &runs,
            &worktrees,
            Some(Path::new("/b/wt-2")),
            HistoryScope::ThisWorktree,
            "",
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(tree.repos.len(), 1);
        assert_eq!(tree.repos[0].label, "beta");
        assert_eq!(tree.total(), 1);
    }

    #[test]
    fn the_active_worktree_opens_by_default_and_carries_the_selection_edge() {
        let runs = vec![run("/a/wt-1", 10), run("/b/wt-2", 20)];
        let worktrees = vec![
            worktree("/a/wt-1", "alpha", "feature-a"),
            worktree("/b/wt-2", "beta", "feature-b"),
        ];
        let tree = build_run_tree(
            &runs,
            &worktrees,
            Some(Path::new("/b/wt-2")),
            HistoryScope::All,
            "",
            &HashMap::new(),
            &HashMap::new(),
        );
        let alpha = &tree.repos[0].groups[0];
        let beta = &tree.repos[1].groups[0];
        assert!(!alpha.is_active);
        assert!(!alpha.open, "an inactive worktree starts folded");
        assert!(beta.is_active);
        assert!(beta.open, "the active worktree opens by default");

        let collapsed: HashMap<PathBuf, bool> = [
            (PathBuf::from("/a/wt-1"), false),
            (PathBuf::from("/b/wt-2"), true),
        ]
        .into_iter()
        .collect();
        let tree = build_run_tree(
            &runs,
            &worktrees,
            Some(Path::new("/b/wt-2")),
            HistoryScope::All,
            "",
            &collapsed,
            &HashMap::new(),
        );
        assert!(tree.repos[0].groups[0].open);
        assert!(!tree.repos[1].groups[0].open);
    }

    #[test]
    fn filtering_narrows_the_rows_but_still_reports_how_much_history_there_really_is() {
        let mut first = run("/a/wt-1", 10);
        first.title = Some("Reproduce the refresh race".to_string());
        let mut second = run("/a/wt-1", 20);
        second.title = Some("Bump axum to 0.8".to_string());
        let worktrees = vec![worktree("/a/wt-1", "alpha", "feature-a")];

        let tree = build_run_tree(
            &[first, second],
            &worktrees,
            Some(Path::new("/a/wt-1")),
            HistoryScope::All,
            "AXUM",
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(tree.total(), 1, "case-insensitive substring on the title");
        assert_eq!(
            tree.unfiltered, 2,
            "the unfiltered count is what tells 'no history' from 'no match'"
        );
    }

    #[test]
    fn drift_is_attached_per_run_and_absent_until_it_is_really_known() {
        let first = run("/a/wt-1", 10);
        let second = run("/a/wt-1", 20);
        let worktrees = vec![worktree("/a/wt-1", "alpha", "feature-a")];
        let drift: HashMap<PathBuf, HashMap<String, usize>> = [(
            PathBuf::from("/a/wt-1"),
            [(first.key.clone(), 4)].into_iter().collect(),
        )]
        .into_iter()
        .collect();

        let tree = build_run_tree(
            &[first, second],
            &worktrees,
            None,
            HistoryScope::All,
            "",
            &HashMap::new(),
            &drift,
        );
        let group = &tree.repos[0].groups[0];
        assert_eq!(group.runs[0].drift, Some(4));
        assert_eq!(group.runs[0].band(), Some(DriftBand::Far));
        assert_eq!(
            group.runs[1].drift, None,
            "an unanswered run paints no band at all rather than a made-up tip"
        );
        assert_eq!(group.runs[1].band(), None);
    }

    // ---------------------------------------------------------------------------- transcripts

    #[test]
    fn a_captured_transcript_is_shown_verbatim_and_still_gets_a_real_closing_line() {
        let mut past = run("/a/wt-1", 1_700_000_000);
        past.ended_at_unix = Some(1_700_000_360);
        past.diffstat = Some(RunDiffstat {
            files: 2,
            insertions: 41,
            deletions: 0,
        });
        let captured = vec![
            "\u{276f} claude".to_string(),
            "\u{25cf} working".to_string(),
        ];
        let body = transcript_body(&past, Some("feature-a"), Some(&captured), 1_700_000_400);

        assert_eq!(body[0].text, "\u{276f} claude");
        assert_eq!(body[1].text, "\u{25cf} working");
        assert_eq!(
            body[body.len() - 2].text,
            "\u{25cf} Finished. 2 files changed, +41 \u{2212}0."
        );
        assert_eq!(
            body[body.len() - 1].text,
            "  \u{2514} run ended today 22:19 after 6m"
        );
    }

    #[test]
    fn a_run_with_no_stored_transcript_describes_only_its_own_record() {
        let mut past = run("/a/wt-1", 1_700_000_000);
        past.title = Some("Move the select builder behind a trait".to_string());
        past.turns = 21;
        past.session_id = Some("3d91e07".to_string());
        past.ended_at_unix = Some(1_700_000_000 + 24 * 60);
        past.diffstat = Some(RunDiffstat {
            files: 6,
            insertions: 148,
            deletions: 96,
        });

        let body = transcript_body(&past, Some("feat/query-builder"), None, 1_700_002_000);
        let text: Vec<&str> = body.iter().map(|line| line.text.as_str()).collect();

        assert_eq!(
            text[0],
            "\u{276f} claude --resume 3d91e07 \u{b7} feat/query-builder"
        );
        assert!(text.contains(&"\u{25cf} Move the select builder behind a trait"));
        assert!(text.contains(&"  \u{2514} 21 turns in feat/query-builder"));
        assert!(text.contains(&"  \u{2514} touched 6 files"));
        assert!(text.contains(&"  \u{2514} no transcript was captured for this run"));
        assert_eq!(
            text[text.len() - 2],
            "\u{25cf} Finished. 6 files changed, +148 \u{2212}96."
        );
    }

    #[test]
    fn an_abandoned_runs_closing_line_says_last_seen_rather_than_claiming_it_ended() {
        let mut past = run("/a/wt-1", 1_700_000_000);
        past.ended_at_unix = None;
        past.updated_at_unix = 1_700_000_500;
        let lines = closing_lines(&past, 1_700_000_600);
        assert_eq!(lines[0].text, "\u{25cf} Left unfinished.");
        assert_eq!(lines[1].text, "  \u{2514} last seen today 22:21");
    }

    #[test]
    fn a_codex_runs_resume_line_is_a_codex_command_not_a_claude_one() {
        let mut past = run("/a/wt-1", 1_700_000_000);
        past.kind = AgentKind::Codex;
        let body = transcript_body(&past, Some("main"), None, 1_700_000_000);
        assert_eq!(body[0].text, "\u{276f} codex \u{b7} main");
    }

    // ----------------------------------------------------------------------------- empty notes

    #[test]
    fn the_empty_note_names_the_branch_when_the_scope_is_one_worktree() {
        assert_eq!(
            empty_note(HistoryScope::ThisWorktree, Some("feat/auth")),
            "No agent has run in feat/auth yet."
        );
        assert_eq!(
            empty_note(HistoryScope::All, Some("feat/auth")),
            "No agent has finished a run yet."
        );
    }

    #[test]
    fn the_count_line_tallies_the_tree_the_body_is_really_showing() {
        let runs = vec![run("/a/wt-1", 10), run("/a/wt-2", 20), run("/b/wt-3", 30)];
        let worktrees = vec![
            worktree("/a/wt-1", "alpha", "feature-a"),
            worktree("/a/wt-2", "alpha", "feature-b"),
            worktree("/b/wt-3", "beta", "main"),
        ];
        let tree = build_run_tree(
            &runs,
            &worktrees,
            None,
            HistoryScope::All,
            "",
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(
            tree.count_line().as_deref(),
            Some("3 runs \u{b7} 3 worktrees")
        );

        // Narrowed by the filter, the line follows - it reports what is on screen, not what is
        // on disk. `tree.unfiltered` is what remembers the difference.
        let narrowed = build_run_tree(
            &runs,
            &worktrees,
            None,
            HistoryScope::All,
            "feature-a",
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(
            narrowed.count_line().as_deref(),
            Some("1 run \u{b7} 1 worktree"),
            "\u{a7}7 rule 9: singular through the helper, on both terms"
        );
        assert_eq!(
            RunTree::default().count_line(),
            None,
            "an empty tree gets its empty note, not a line of zeroes"
        );
    }

    #[test]
    fn the_rail_link_says_one_earlier_run_in_the_singular() {
        assert_eq!(earlier_runs_label(1), "1 earlier run");
        assert_eq!(earlier_runs_label(2), "2 earlier runs");
    }

    #[test]
    fn the_filtered_away_note_pluralises_through_the_one_helper() {
        assert_eq!(filtered_away_note(1), "No match in the 1 run.");
        assert_eq!(filtered_away_note(9), "No match in the 9 runs.");
    }
}
