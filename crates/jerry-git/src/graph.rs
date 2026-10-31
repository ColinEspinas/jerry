//! Read-only commit-graph data: a commit list with each row assigned a lane, plus the refs
//! pointing at each commit.
//!
//! The walk is topological, not commit-time sorted: same-second parent/child pairs are routine,
//! and a parent emitted first makes [`layout_lanes`] drop that edge and paint a floating dot.
//! Sorting the capped set afterwards instead would break prefix stability across `max_commits`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

use gix::traverse::commit::topo;
use gix::traverse::commit::Parents;
use gix::ObjectId;

use crate::diff::AheadBehind;
use crate::error::Error;
use crate::{check_success, is_dirty, list_worktrees, open_repo, run_git};

/// Cap on how many commits one [`build_graph`] call loads; beyond it the history is truncated.
pub const DEFAULT_MAX_COMMITS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphScope {
    /// Every local branch, remote-tracking branch and tag.
    #[default]
    All,
    /// Only branches checked out in one of this repository's worktrees.
    Worktrees,
    /// Only `HEAD`'s first-parent ancestry, mirroring `git log --first-parent`.
    Current,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChip {
    /// Short display name: `main`, `origin/main`, `v1.0`.
    pub name: String,
    pub kind: RefKind,
    pub is_head: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitNode {
    pub id: String,
    pub short_id: String,
    pub parent_ids: Vec<String>,
    pub subject: String,
    /// The message body after the subject line, if any (empty when the commit has no body).
    pub body: String,
    pub author_name: String,
    pub author_email: String,
    pub author_time_unix: i64,
    pub committer_time_unix: i64,
    /// Refs pointing directly at this commit, if any.
    pub refs: Vec<RefChip>,
}

impl CommitNode {
    pub fn is_merge(&self) -> bool {
        self.parent_ids.len() > 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotKind {
    Commit,
    Head,
    Merge,
    /// The synthetic first row representing uncommitted changes in the worktree.
    WorkingTree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneSegment {
    pub lane: usize,
    /// This lane begins at this row (draw only the bottom half).
    pub starts_here: bool,
    /// This lane ends at this row (draw only the top half).
    pub ends_here: bool,
    /// The segment below this row's dot is dashed; only the working-tree row's lane.
    pub dashed: bool,
}

/// Which of the two elbow shapes an [`Elbow`] draws. They are mirrors, occupying opposite halves
/// of the row box; a row can need either or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElbowKind {
    /// This row is a merge commit: `from_lane` is its own dot, curving down into `to_lane`, which
    /// continues below. Drawn in the row's bottom half.
    Diverging,
    /// This row's commit is also the next expected commit for another lane - two branches sharing
    /// an ancestor, no merge involved. `from_lane` is that other, ending lane, curving over to
    /// join `to_lane`, this row's dot. Drawn in the row's top half.
    Converging,
}

/// `from_lane`/`to_lane` are not symmetric between the two [`ElbowKind`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elbow {
    pub from_lane: usize,
    pub to_lane: usize,
    pub kind: ElbowKind,
}

/// One row of the graph, in commit order (newest first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    pub commit: CommitNode,
    pub lane: usize,
    pub dot_kind: DotKind,
    pub lane_segments: Vec<LaneSegment>,
    pub elbows: Vec<Elbow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Graph {
    pub rows: Vec<GraphRow>,
    /// One past the highest lane index used - the lane canvas's required width in lanes.
    pub lane_count: usize,
    /// `true` if the walk was stopped early by [`DEFAULT_MAX_COMMITS`] (or a caller-supplied cap).
    pub truncated: bool,
}

/// Builds the commit graph for `repo_path`, scoped per [`GraphScope`]. `max_commits` of `0` uses
/// [`DEFAULT_MAX_COMMITS`].
pub fn build_graph(
    repo_path: &Path,
    scope: GraphScope,
    max_commits: usize,
) -> Result<Graph, Error> {
    let repo = open_repo(repo_path)?;
    let max_commits = if max_commits == 0 {
        DEFAULT_MAX_COMMITS
    } else {
        max_commits
    };

    let mut head = repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    let head_branch = head.referent_name().map(|name| name.shorten().to_string());
    let head_id = head
        .try_peel_to_id_in_place()
        .map_err(|source| Error::PeelHead(Box::new(source)))?
        .map(|id| id.detach());

    let (refs_by_commit, all_tips) = collect_refs(&repo, head_branch.as_deref())?;

    let tips: Vec<ObjectId> = match scope {
        GraphScope::Current => head_id.into_iter().collect(),
        GraphScope::All => all_tips,
        GraphScope::Worktrees => collect_worktree_tips(repo_path, &repo)?,
    };

    if tips.is_empty() {
        return Ok(Graph::default());
    }

    // Several refs legitimately point at one commit, and the topo walk seeds one queue entry per
    // tip it is handed - so a duplicate would emit its commit as two rows.
    let mut seen_tips = std::collections::HashSet::with_capacity(tips.len());
    let tips: Vec<ObjectId> = tips
        .into_iter()
        .filter(|tip| seen_tips.insert(*tip))
        .collect();

    // The commit-graph file is a pure accelerator: without it the walk still reads correctly from
    // the object database, so its absence must not fail the build.
    let mut builder = topo::Builder::from_iters(&repo.objects, tips, None::<Vec<ObjectId>>)
        .sorting(topo::Sorting::DateOrder)
        .with_commit_graph(repo.commit_graph_if_enabled().ok().flatten());
    if matches!(scope, GraphScope::Current) {
        builder = builder.parents(Parents::First);
    }
    let walk = builder
        .build()
        .map_err(|source| Error::RevWalk(Box::new(source)))?;

    let mut edges: Vec<(ObjectId, Vec<ObjectId>)> = Vec::new();
    let mut nodes: HashMap<ObjectId, CommitNode> = HashMap::new();
    let mut truncated = false;
    for (count, info) in walk.enumerate() {
        if count >= max_commits {
            truncated = true;
            break;
        }
        let info = info.map_err(|source| Error::RevWalkIter(Box::new(source)))?;
        let commit = repo
            .find_commit(info.id)
            .map_err(|source| Error::RevWalkObject(Box::new(source)))?;
        let mut parent_ids: Vec<ObjectId> = info.parent_ids.iter().copied().collect();
        if matches!(scope, GraphScope::Current) {
            // `Parents::First` restricts the traversal but the walk still reports every parent on
            // each commit. Trimming them keeps rows first-parent-shaped: otherwise `layout_lanes`
            // opens a lane for a merge's second parent this scope never visits, drawing a line
            // that runs off the bottom of the graph forever.
            parent_ids.truncate(1);
        }
        let refs = refs_by_commit.get(&info.id).cloned().unwrap_or_default();
        let node = commit_node(&info.id, &commit, refs)?;
        edges.push((info.id, parent_ids));
        nodes.insert(info.id, node);
    }

    let layouts = layout_lanes(&edges);
    let mut rows: Vec<GraphRow> = Vec::with_capacity(edges.len());
    for ((id, _parents), layout) in edges.iter().zip(layouts) {
        // Populated 1:1 above, so this always matches; skipping beats panicking if it ever
        // stops being true.
        let Some(commit) = nodes.remove(id) else {
            continue;
        };
        // Merge-ness is checked before HEAD-ness because a commit can be both, and merge is the
        // structurally significant one to keep. `RefChip::is_head` already marks HEAD on the row.
        let dot_kind = if commit.is_merge() {
            DotKind::Merge
        } else if Some(*id) == head_id {
            DotKind::Head
        } else {
            DotKind::Commit
        };
        rows.push(GraphRow {
            commit,
            lane: layout.lane,
            dot_kind,
            lane_segments: layout.segments,
            elbows: layout.elbows,
        });
    }

    // Only added when the worktree is dirty *and* `HEAD` is the first row - skipped, not faked,
    // when a newer commit on another branch legitimately sorts first.
    //
    // Named "Working tree" rather than "Uncommitted changes" because the Changes panel already
    // has a section by that name; this row is the working tree as a point on the graph.
    if let Some(first) = rows.first() {
        if Some(&first.commit.id) == head_id.map(|id| id.to_string()).as_ref()
            && is_dirty(repo_path).unwrap_or(false)
        {
            let lane = first.lane;
            let working_tree_row = GraphRow {
                commit: CommitNode {
                    id: String::new(),
                    short_id: String::new(),
                    parent_ids: vec![first.commit.id.clone()],
                    subject: "Working tree".to_string(),
                    body: String::new(),
                    author_name: String::new(),
                    author_email: String::new(),
                    author_time_unix: 0,
                    committer_time_unix: 0,
                    refs: Vec::new(),
                },
                lane,
                dot_kind: DotKind::WorkingTree,
                lane_segments: vec![LaneSegment {
                    lane,
                    starts_here: true,
                    ends_here: false,
                    dashed: true,
                }],
                elbows: Vec::new(),
            };
            rows.insert(0, working_tree_row);
        }
    }

    let lane_count = rows
        .iter()
        .map(|row| row.lane)
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);
    Ok(Graph {
        rows,
        lane_count,
        truncated,
    })
}

fn commit_node(
    id: &ObjectId,
    commit: &gix::Commit<'_>,
    refs: Vec<RefChip>,
) -> Result<CommitNode, Error> {
    let short_id = commit
        .short_id()
        .map(|prefix| prefix.to_string())
        .unwrap_or_else(|_| id.to_string()[..7.min(id.to_string().len())].to_string());
    let message = commit
        .message()
        .map_err(|source| Error::RevWalkDecode(Box::new(source)))?;
    let subject = message.summary().to_string();
    let body = message
        .body
        .map(|body| body.to_string())
        .unwrap_or_default();
    let author = commit
        .author()
        .map_err(|source| Error::RevWalkDecode(Box::new(source)))?;
    let committer_time = commit
        .time()
        .map_err(|source| Error::RevWalkCommit(Box::new(source)))?;

    Ok(CommitNode {
        id: id.to_string(),
        short_id,
        parent_ids: commit.parent_ids().map(|id| id.to_string()).collect(),
        subject,
        body,
        author_name: author.name.to_string(),
        author_email: author.email.to_string(),
        author_time_unix: author.time.seconds,
        committer_time_unix: committer_time.seconds,
        refs,
    })
}

const MAX_COMMIT_FILES: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFileChange {
    pub path: std::path::PathBuf,
    pub status: crate::diff::FileChangeStatus,
}

/// A root commit diffs against the empty tree; a merge reports nothing at all, which is git's own
/// default rather than this function guessing which parent to diff against.
///
/// `commit_sha` is checked as hex before reaching a `git` argument.
pub fn commit_changed_files(
    repo_path: &Path,
    commit_sha: &str,
) -> Result<Vec<CommitFileChange>, Error> {
    if commit_sha.is_empty() || !commit_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::WorktreeIo(std::io::Error::other(
            "commit id was not a hex object id",
        )));
    }
    let args: Vec<OsString> = vec![
        "show".into(),
        "--format=".into(),
        "--name-status".into(),
        commit_sha.into(),
    ];
    let output = run_git(repo_path, &args)?;
    check_success(&args, &output)?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_name_status(&text))
}

fn parse_name_status(text: &str) -> Vec<CommitFileChange> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || out.len() >= MAX_COMMIT_FILES {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let Some(code) = parts.next() else { continue };
        let Some(rest) = parts.next() else { continue };
        let status = match code.as_bytes().first() {
            Some(b'A') => crate::diff::FileChangeStatus::Added,
            Some(b'D') => crate::diff::FileChangeStatus::Deleted,
            Some(b'R') => crate::diff::FileChangeStatus::Renamed,
            _ => crate::diff::FileChangeStatus::Modified,
        };
        // A rename/copy line is `R100\told\tnew`; the destination is what exists in the tree.
        let path = rest.rsplit('\t').next().unwrap_or(rest);
        out.push(CommitFileChange {
            path: std::path::PathBuf::from(path),
            status,
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommit {
    pub id: String,
    /// git's own `%h` abbreviation of [`Self::id`], never a hand-truncated prefix.
    pub short_id: String,
}

/// Resolves a branch name to its tip commit, through `gix` rather than `git log`/`git rev-parse`
/// because neither can be guarded
/// the way [`crate::checkout::checkout_branch`] guards its positional: both overload a trailing
/// `--` to mean "pathspec follows", so `git log -1 -- feature` exits 0 with no output rather than
/// resolving the branch. Nor does putting the name first help - git's option parser consumes
/// `--evil` as a flag before the `--` is reached. A `find_reference` lookup key has no command
/// line to be misparsed as part of.
///
/// A `branch` naming nothing is reported via [`Error::WorktreeIo`].
pub fn resolve_commit(worktree_path: &Path, branch: &str) -> Result<ResolvedCommit, Error> {
    let repo = crate::open_repo(worktree_path)?;
    let full_ref_name = format!("refs/heads/{branch}");
    let mut reference = repo.find_reference(full_ref_name.as_str()).map_err(|_| {
        Error::WorktreeIo(std::io::Error::other(format!("no such branch: {branch:?}")))
    })?;
    let id = reference
        .peel_to_id_in_place()
        .map_err(|source| Error::PeelReference(Box::new(source)))?
        .detach();
    let full_hex = id.to_string();

    let args: Vec<OsString> = vec![
        "rev-parse".into(),
        "--short".into(),
        full_hex.clone().into(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    let short_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if short_id.is_empty() {
        return Err(Error::WorktreeIo(std::io::Error::other(format!(
            "could not read a short id for {full_hex} out of `git rev-parse --short` output"
        ))));
    }

    Ok(ResolvedCommit {
        id: full_hex,
        short_id,
    })
}

/// Counts against `HEAD`'s configured `@{upstream}` - a different comparison from
/// [`crate::diff::ahead_behind_against_base`], which uses the detected default branch. Push/pull
/// counts are about the remote-tracking ref, not the merge target.
///
/// `Ok(None)`, not `{0, 0}`, when there is no configured upstream.
pub fn ahead_behind_against_upstream(worktree_path: &Path) -> Result<Option<AheadBehind>, Error> {
    let repo = open_repo(worktree_path)?;
    let mut head = repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    if head.try_peel_to_id_in_place().is_err() {
        return Ok(None);
    }
    if head
        .try_peel_to_id_in_place()
        .map_err(|source| Error::PeelHead(Box::new(source)))?
        .is_none()
    {
        // Unborn HEAD.
        return Ok(None);
    }

    // Asking git to resolve `@{upstream}` beats reading `branch.<name>.merge`/`.remote` by hand:
    // an unusual remote setup then resolves the same way `git pull`/`git push` would see it.
    let upstream_args: Vec<OsString> = vec![
        "rev-parse".into(),
        "--abbrev-ref".into(),
        "--symbolic-full-name".into(),
        "@{upstream}".into(),
    ];
    let upstream_output = run_git(worktree_path, &upstream_args)?;
    if !upstream_output.status.success() {
        return Ok(None);
    }

    let args: Vec<OsString> = vec![
        "rev-list".into(),
        "--left-right".into(),
        "--count".into(),
        "@{upstream}...HEAD".into(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_counts(&text))
}

/// Which of `commits` are already pushed to `HEAD`'s `@{upstream}`.
///
/// [`ahead_behind_against_upstream`]'s aggregate counts do not say *which* commits diverge, which
/// a per-commit warning needs.
///
/// `Ok(None)` with no configured upstream, rather than an empty `Vec` that would be
/// indistinguishable from "checked, none pushed".
///
/// One `git merge-base --is-ancestor` per commit; anything but its 0/1 exit is surfaced as an
/// error rather than read as "no".
pub fn commits_already_on_upstream(
    worktree_path: &Path,
    commits: &[String],
) -> Result<Option<Vec<String>>, Error> {
    let upstream_args: Vec<OsString> = vec![
        "rev-parse".into(),
        "--abbrev-ref".into(),
        "--symbolic-full-name".into(),
        "@{upstream}".into(),
    ];
    let upstream_output = run_git(worktree_path, &upstream_args)?;
    if !upstream_output.status.success() {
        return Ok(None);
    }
    let upstream = String::from_utf8_lossy(&upstream_output.stdout)
        .trim()
        .to_string();

    let mut already_on_upstream = Vec::new();
    for commit in commits {
        let args: Vec<OsString> = vec![
            "merge-base".into(),
            "--is-ancestor".into(),
            commit.clone().into(),
            upstream.clone().into(),
        ];
        let output = run_git(worktree_path, &args)?;
        match output.status.code() {
            Some(0) => already_on_upstream.push(commit.clone()),
            Some(1) => {}
            _ => {
                return Err(Error::GitCommand {
                    args: crate::format_args(&args),
                    exit: crate::error::GitExit::from_status(&output.status),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
        }
    }
    Ok(Some(already_on_upstream))
}

fn parse_counts(text: &str) -> Option<AheadBehind> {
    let mut parts = text.split_whitespace();
    let behind = parts.next().and_then(|part| part.parse::<usize>().ok())?;
    let ahead = parts.next().and_then(|part| part.parse::<usize>().ok())?;
    Some(AheadBehind { ahead, behind })
}

type RefIndex = (HashMap<ObjectId, Vec<RefChip>>, Vec<ObjectId>);

/// Annotated tags are peeled and `refs/remotes/*/HEAD` is skipped, so a tag chip always names a
/// commit and no phantom `origin/HEAD` chip appears.
fn collect_refs(repo: &gix::Repository, head_branch: Option<&str>) -> Result<RefIndex, Error> {
    let mut by_commit: HashMap<ObjectId, Vec<RefChip>> = HashMap::new();
    let mut tips: Vec<ObjectId> = Vec::new();

    let platform = repo
        .references()
        .map_err(|source| Error::References(Box::new(source)))?;
    let iter = platform
        .all()
        .map_err(|source| Error::ReferencesIter(Box::new(source)))?;
    for reference in iter {
        let mut reference = reference.map_err(Error::ReferenceEntry)?;
        let full_name = reference.name().as_bstr().to_string();

        let (kind, short_name) = if let Some(short) = full_name.strip_prefix("refs/heads/") {
            (RefKind::LocalBranch, short.to_string())
        } else if let Some(short) = full_name.strip_prefix("refs/remotes/") {
            if short.ends_with("/HEAD") {
                continue;
            }
            (RefKind::RemoteBranch, short.to_string())
        } else if let Some(short) = full_name.strip_prefix("refs/tags/") {
            (RefKind::Tag, short.to_string())
        } else {
            continue;
        };

        let Ok(id) = reference.peel_to_id_in_place() else {
            continue;
        };
        let id = id.detach();

        if matches!(kind, RefKind::LocalBranch | RefKind::RemoteBranch) {
            tips.push(id);
        }

        let is_head = kind == RefKind::LocalBranch && head_branch == Some(short_name.as_str());
        by_commit.entry(id).or_default().push(RefChip {
            name: short_name,
            kind,
            is_head,
        });
    }

    Ok((by_commit, tips))
}

/// Tips for [`GraphScope::Worktrees`]: the `HEAD` of every worktree that has one, main included.
fn collect_worktree_tips(repo_path: &Path, repo: &gix::Repository) -> Result<Vec<ObjectId>, Error> {
    let worktrees = list_worktrees(repo_path)?;
    let mut tips = Vec::new();
    for worktree in worktrees.into_iter().flatten() {
        let Some(head_commit) = worktree.head_commit else {
            continue;
        };
        if let Ok(id) = gix::ObjectId::from_hex(head_commit.as_bytes()) {
            tips.push(id);
        }
    }
    // A worktree's `head_commit` may name an object this `repo` handle cannot resolve. Verifying
    // costs a round-trip per tip for nothing: `rev_walk` already tolerates an unknown tip.
    let _ = repo;
    Ok(tips)
}

/// One row's layout output from [`layout_lanes`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowLayout {
    lane: usize,
    segments: Vec<LaneSegment>,
    elbows: Vec<Elbow>,
}

/// Assigns each commit a lane and describes each row's segments and elbows, given commits newest
/// first with their parent ids.
///
/// Pure and generic over the id type, so it is testable with `&str` ids and independent of `gix`.
///
/// Assumes topologically sound input - every commit before its parents - which [`build_graph`]
/// guarantees. If that is ever violated, a parent edge pointing at an already-emitted row is
/// dropped rather than leaving a lane permanently expecting a commit it will never see: a
/// degraded rendering instead of a silently wrong one.
fn layout_lanes<Id: Clone + Eq + std::hash::Hash>(commits: &[(Id, Vec<Id>)]) -> Vec<RowLayout> {
    let mut lanes: Vec<Option<Id>> = Vec::new();
    let mut seen: std::collections::HashSet<Id> =
        std::collections::HashSet::with_capacity(commits.len());
    let mut out = Vec::with_capacity(commits.len());

    for (id, parents) in commits {
        seen.insert(id.clone());
        // Lanes free *before* this row changes anything. Step 4 must only reuse one of these,
        // never a lane this same row just freed: that yields either a self-loop elbow, or two
        // segments for one lane index in one row, which paints two unrelated branches as a single
        // unbroken line.
        let free_before_row: std::collections::HashSet<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_none())
            .map(|(index, _)| index)
            .collect();

        // Step 1: find (or allocate) this commit's own lane.
        let mut own_lane = lanes.iter().position(|slot| slot.as_ref() == Some(id));
        let own_lane_is_new = own_lane.is_none();
        let own_lane = *own_lane.get_or_insert_with(|| allocate_lane(&mut lanes, id.clone()));

        // Step 2: any *other* lane expecting this same commit - branches converging on a shared
        // ancestor without this row being a merge - collapses into `own_lane` and ends here. Each
        // gets a `Converging` elbow; without it the ending lane's top-half stub has nothing
        // joining it to this row's dot.
        //
        // No out-of-order filtering is needed here: Step 3/4 already refuse to point a lane at an
        // id that has been emitted, so every lane found here was validly waiting for `id`.
        let mut ends_here_lanes: Vec<usize> = Vec::new();
        let mut elbows = Vec::new();
        for (index, slot) in lanes.iter_mut().enumerate() {
            if index == own_lane {
                continue;
            }
            if slot.as_ref() == Some(id) {
                *slot = None;
                ends_here_lanes.push(index);
                elbows.push(Elbow {
                    from_lane: index,
                    to_lane: own_lane,
                    kind: ElbowKind::Converging,
                });
            }
        }

        // Snapshot which lanes are active (pre-update) for this row's "through" segments.
        let through_lanes: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(index, slot)| *index != own_lane && slot.is_some())
            .map(|(index, _)| index)
            .collect();

        // Step 3: point `own_lane` at the first parent, or free it for a root commit or a parent
        // already emitted out of order.
        let first_parent = parents.first().cloned();
        let first_parent_already_seen = first_parent.as_ref().is_some_and(|p| seen.contains(p));
        let own_ends_here = first_parent.is_none() || first_parent_already_seen;
        lanes[own_lane] = if first_parent_already_seen {
            None
        } else {
            first_parent
        };

        // Step 4: further parents either reuse a tracked lane (elbow only) or open a new one
        // (elbow plus a started lane). A parent already emitted out of order is skipped.
        let mut new_lane_segments = Vec::new();
        for parent in parents.iter().skip(1) {
            if seen.contains(parent) {
                continue;
            }
            if let Some(existing) = lanes
                .iter()
                .enumerate()
                .position(|(index, slot)| index != own_lane && slot.as_ref() == Some(parent))
            {
                elbows.push(Elbow {
                    from_lane: own_lane,
                    to_lane: existing,
                    kind: ElbowKind::Diverging,
                });
            } else {
                let new_lane =
                    allocate_fresh_lane(&mut lanes, &free_before_row, own_lane, parent.clone());
                elbows.push(Elbow {
                    from_lane: own_lane,
                    to_lane: new_lane,
                    kind: ElbowKind::Diverging,
                });
                new_lane_segments.push(LaneSegment {
                    lane: new_lane,
                    starts_here: true,
                    ends_here: false,
                    dashed: false,
                });
            }
        }

        let mut segments: Vec<LaneSegment> = through_lanes
            .into_iter()
            .map(|lane| LaneSegment {
                lane,
                starts_here: false,
                ends_here: false,
                dashed: false,
            })
            .collect();
        segments.extend(ends_here_lanes.into_iter().map(|lane| LaneSegment {
            lane,
            starts_here: false,
            ends_here: true,
            dashed: false,
        }));
        segments.push(LaneSegment {
            lane: own_lane,
            starts_here: own_lane_is_new,
            ends_here: own_ends_here,
            dashed: false,
        });
        segments.extend(new_lane_segments);
        segments.sort_by_key(|segment| segment.lane);

        out.push(RowLayout {
            lane: own_lane,
            segments,
            elbows,
        });
    }

    out
}

/// Reuses the first free lane slot or appends one, which is how a lane index gets recycled across
/// unrelated branches later in the same history.
fn allocate_lane<Id>(lanes: &mut Vec<Option<Id>>, expecting: Id) -> usize {
    if let Some(index) = lanes.iter().position(|slot| slot.is_none()) {
        lanes[index] = Some(expecting);
        index
    } else {
        lanes.push(Some(expecting));
        lanes.len() - 1
    }
}

/// [`allocate_lane`] for Step 4's new-lane case: reuses a slot only if it was free before this
/// row began and is not `own_lane`, never one this row just freed itself.
fn allocate_fresh_lane<Id>(
    lanes: &mut Vec<Option<Id>>,
    free_before_row: &std::collections::HashSet<usize>,
    own_lane: usize,
    expecting: Id,
) -> usize {
    if let Some(index) = lanes.iter().enumerate().position(|(index, slot)| {
        slot.is_none() && free_before_row.contains(&index) && index != own_lane
    }) {
        lanes[index] = Some(expecting);
        index
    } else {
        lanes.push(Some(expecting));
        lanes.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;
    use test_support::{git, git_output, seed_empty_repo};

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) {
        fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", message]);
    }

    /// [`commit`] with an explicit timestamp, so a multi-tip history is well-ordered
    /// deterministically rather than depending on the test crossing a wall-clock second.
    fn commit_at(dir: &Path, file: &str, contents: &str, message: &str, unix_seconds: i64) {
        fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        let date = format!("{unix_seconds} +0000");
        let output = Command::new("git")
            .current_dir(dir)
            .args(["commit", "-m", message])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .expect("failed to spawn git commit");
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `git merge` with an explicit timestamp; see [`commit_at`].
    fn merge_at(dir: &Path, branch: &str, message: &str, unix_seconds: i64) {
        let date = format!("{unix_seconds} +0000");
        let output = Command::new("git")
            .current_dir(dir)
            .args(["merge", "--no-ff", branch, "-m", message])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .expect("failed to spawn git merge");
        assert!(
            output.status.success(),
            "git merge failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn resolve_commit_reports_a_branchs_real_tip_in_both_forms() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["checkout", "-b", "feature"]);
        commit(repo.path(), "b.txt", "feature", "feature work");
        let feature_tip = String::from_utf8_lossy(
            &Command::new("git")
                .current_dir(repo.path())
                .args(["rev-parse", "feature"])
                .output()
                .expect("git rev-parse")
                .stdout,
        )
        .trim()
        .to_string();
        git(repo.path(), &["checkout", "main"]);

        let resolved = resolve_commit(repo.path(), "feature").expect("resolve_commit");

        assert_eq!(
            resolved.id, feature_tip,
            "must resolve to the branch's own real tip commit, not HEAD's"
        );
        assert!(
            !resolved.short_id.is_empty() && feature_tip.starts_with(&resolved.short_id),
            "the short form must be git's own real abbreviation of that same commit: {resolved:?}"
        );
    }

    #[test]
    fn resolve_commit_refuses_to_fabricate_a_commit_for_a_branch_that_does_not_exist() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");

        let err = resolve_commit(repo.path(), "no-such-branch").expect_err(
            "a branch that names nothing must be a real error, not a fabricated commit",
        );
        match err {
            Error::WorktreeIo(io_err) => assert!(
                io_err.to_string().contains("no-such-branch"),
                "the error must name the unresolved branch so the caller can show it: {io_err}"
            ),
            other => panic!("expected Error::WorktreeIo, got {other:?}"),
        }
    }

    #[test]
    fn resolve_commit_treats_a_flag_shaped_branch_name_as_an_ordinary_ref_lookup() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");

        let err = resolve_commit(repo.path(), "--evil").expect_err(
            "a flag-shaped name must still be refused as unresolved, not act as a flag",
        );
        match err {
            Error::WorktreeIo(io_err) => assert!(
                io_err.to_string().contains("--evil"),
                "must be the ordinary \"no such branch\" refusal naming it, not anything that \
                 implies a subprocess argument was involved: {io_err}"
            ),
            other => panic!("expected Error::WorktreeIo, got {other:?}"),
        }
    }

    // ---- layout_lanes: pure, gix-independent ----

    #[test]
    fn linear_history_stays_on_lane_zero() {
        let commits = vec![("c3", vec!["c2"]), ("c2", vec!["c1"]), ("c1", vec![])];
        let layout = layout_lanes(&commits);
        assert_eq!(layout.len(), 3);
        for row in &layout {
            assert_eq!(row.lane, 0, "linear history must stay on lane 0");
            assert!(row.elbows.is_empty());
        }
        assert!(layout[2]
            .segments
            .iter()
            .any(|s| s.lane == 0 && s.ends_here));
        // The first row starts its lane, which is expected; what matters is that no row in the
        // middle of a linear chain claims to.
        assert!(!layout[1].segments.iter().any(|s| s.starts_here));
        assert!(!layout[2].segments.iter().any(|s| s.starts_here));
    }

    #[test]
    fn layout_lanes_degrades_gracefully_when_a_parent_was_already_shown_out_of_order() {
        // `merge`'s second parent was already emitted as an earlier row. This must not panic,
        // loop, or dangle a lane; the edge is simply omitted.
        let commits: Vec<(&str, Vec<&str>)> = vec![
            ("feature", vec!["base"]),
            ("merge", vec!["base", "feature"]),
            ("base", vec![]),
        ];
        let layout = layout_lanes(&commits);
        assert_eq!(layout.len(), 3);
        assert!(
            layout[1].elbows.is_empty(),
            "the merge's edge to the already-shown `feature` commit must be omitted, not left \
             pointing at a lane that will never resolve"
        );
        let base_row = &layout[2];
        assert!(base_row
            .segments
            .iter()
            .any(|s| s.ends_here && s.lane == base_row.lane));
        assert!(
            base_row.segments.iter().filter(|s| s.ends_here).count() >= 1,
            "at least the row's own lane must end at the root commit"
        );
        // `merge`'s lane, still waiting for `base` as its first parent, reaches
        // `base` here (unlike the skipped `feature` edge above) - it must get a real Converging
        // elbow into `base_row`'s own lane, not just an ends_here stub with nothing connecting it.
        assert!(
            base_row
                .elbows
                .iter()
                .any(|e| e.kind == ElbowKind::Converging
                    && e.to_lane == base_row.lane
                    && e.from_lane != base_row.lane),
            "the merge lane reaching the shared root must get a real connecting elbow: {:?}",
            base_row.elbows
        );
    }

    #[test]
    fn branch_and_merge_opens_and_closes_a_second_lane() {
        let commits: Vec<(&str, Vec<&str>)> = vec![
            ("c4", vec!["c3", "c2b"]),
            ("c3", vec!["c1"]),
            ("c2b", vec!["c1"]),
            ("c1", vec![]),
        ];
        let layout = layout_lanes(&commits);
        assert_eq!(layout[0].lane, 0, "merge commit renders on the trunk lane");
        assert_eq!(
            layout[0].elbows,
            vec![Elbow {
                from_lane: 0,
                to_lane: 1,
                kind: ElbowKind::Diverging,
            }],
            "merge commit opens an elbow onto a new lane for its second parent"
        );
        assert_eq!(layout[1].lane, 0, "trunk continues on lane 0");
        assert_eq!(
            layout[2].lane, 1,
            "feature commit sits on the newly opened lane"
        );
        // c1 is reached by both lane 0 (via c3) and lane 1 (via c2b) - lane 1 must collapse into
        // lane 0 and end there, not open a second copy of c1.
        assert_eq!(layout[3].lane, 0);
        assert!(
            layout[3]
                .segments
                .iter()
                .any(|s| s.lane == 1 && s.ends_here),
            "the feature lane must end where it rejoins the trunk"
        );
        assert_eq!(
            layout[3].elbows,
            vec![Elbow {
                from_lane: 1,
                to_lane: 0,
                kind: ElbowKind::Converging,
            }],
            "the ending feature lane must get a real elbow connecting it to c1's own dot, not \
             just a dangling stub"
        );
    }

    #[test]
    fn two_independent_tips_sharing_an_ancestor_both_get_converging_elbows() {
        // Regression test for the real bug found in this repository's own history (row 9,
        // commit `ac8e6cd`): two entirely independent branch tips (walked as separate DAG roots
        // by `GraphScope::All`, exactly like `d` and `e` below) share a common ancestor `shared`
        // as their *own* first parent - `shared` is not itself a merge commit (one parent, no
        // `Diverging` elbow of its own), but two other, unrelated lanes both end there. Every
        // ending lane must get a real `Converging` elbow into `shared`'s own lane, not an empty
        // `elbows` vec with a dangling stub (the bug this branch fixes).
        let commits: Vec<(&str, Vec<&str>)> = vec![
            ("d", vec!["shared"]),
            ("e", vec!["shared"]),
            ("shared", vec![]),
        ];
        let layout = layout_lanes(&commits);
        assert_eq!(layout[0].lane, 0, "d opens lane 0");
        assert_eq!(layout[1].lane, 1, "e opens lane 1 (unrelated to d's lane)");
        assert!(
            layout[0].elbows.is_empty() && layout[1].elbows.is_empty(),
            "neither d nor e is itself a merge, so neither row gets its own elbow"
        );

        let shared_row = &layout[2];
        // `shared`'s own lane reuses whichever of lane 0/1 it's found on first (lane 0, since d
        // was processed first) - the *other* lane (1) must end here and get a real Converging
        // elbow into `shared`'s own lane, exactly like this repo's real row 9.
        assert_eq!(shared_row.lane, 0);
        assert!(
            shared_row
                .segments
                .iter()
                .any(|s| s.lane == 1 && s.ends_here),
            "lane 1 must end at the shared ancestor"
        );
        assert_eq!(
            shared_row.elbows,
            vec![Elbow {
                from_lane: 1,
                to_lane: 0,
                kind: ElbowKind::Converging,
            }],
            "the ending lane must get a real connecting elbow, not an empty elbows vec with a \
             dangling stub - this is the exact bug found in this repository's own row 9"
        );
    }

    #[test]
    fn a_merge_row_that_is_also_a_shared_ancestor_gets_both_elbow_kinds() {
        // Regression test for the real bug's second confirmed instance (row 14, "Merge pull
        // request #23"): a row can be a genuine merge commit (its own real Diverging elbow) *and*
        // simultaneously the shared ancestor an unrelated, independent lane converges on - the
        // two must coexist correctly, each in its own kind, without one clobbering the other.
        //
        // `m` merges `c1` and `c2`; independently, *two* unrelated tips (`other1`, `other2`) each
        // have `m` as their own first parent - so two separate lanes both end exactly at `m`'s
        // row (own_lane reuses one of them; the other must collapse in via a real Converging
        // elbow), at the very same time `m`'s own second parent opens a real Diverging elbow -
        // exactly the shape found in this repository's real row 14 (a real merge commit whose
        // `lane_segments` *also* showed an unrelated lane ending there with no elbow at all).
        let commits: Vec<(&str, Vec<&str>)> = vec![
            ("other1", vec!["m"]),
            ("other2", vec!["m"]),
            ("m", vec!["c1", "c2"]),
            ("c1", vec![]),
            ("c2", vec![]),
        ];
        let layout = layout_lanes(&commits);
        assert_eq!(layout[0].lane, 0, "other1 opens lane 0");
        assert_eq!(layout[1].lane, 1, "other2 opens lane 1");

        let m_row = &layout[2];
        assert_eq!(m_row.lane, 0, "m reuses other1's now-ending lane 0");
        assert_eq!(
            m_row.elbows.len(),
            2,
            "m must have both its own real Diverging merge elbow and a Converging elbow for \
             other2's ending lane: {:?}",
            m_row.elbows
        );
        assert!(
            m_row.elbows.iter().any(|e| e.kind == ElbowKind::Converging
                && e.from_lane == 1
                && e.to_lane == m_row.lane),
            "other2's ending lane must get a real Converging elbow into m's own dot, not a \
             dangling stub: {:?}",
            m_row.elbows
        );
        assert!(
            m_row.elbows.iter().any(|e| e.kind == ElbowKind::Diverging
                && e.from_lane == m_row.lane
                && e.to_lane != m_row.lane),
            "m's own second parent must still get its real Diverging elbow into a genuinely \
             different lane, unaffected by the converging case (a self-loop elbow, from_lane == \
             to_lane, would be exactly as broken as no elbow at all): {:?}",
            m_row.elbows
        );
        // Regression for a real gap an adversarial audit found in this fix: Step 4's own
        // "allocate a new lane" search must never reuse the very lane Step 2 just freed on this
        // same row (`other2`'s ending lane 1) - doing so would give lane 1 *two* `LaneSegment`s
        // in the same row (an `ends_here` one from Step 2, a `starts_here` one from Step 4),
        // which paints as one unbroken pass-through line for what are really two unrelated
        // branches. Every lane index in a single row's `segments` must be unique.
        let mut lanes_seen_this_row = std::collections::HashSet::new();
        for segment in &m_row.segments {
            assert!(
                lanes_seen_this_row.insert(segment.lane),
                "lane {} has more than one LaneSegment in the same row: {:?}",
                segment.lane,
                m_row.segments
            );
        }
    }

    #[test]
    fn a_merge_whose_first_parent_was_already_seen_never_self_loops_its_second_parent() {
        // Regression for a real bug an adversarial audit found: when a merge row's *first*
        // parent has already been seen (an out-of-order history - see `layout_lanes`'s own
        // "Out-of-order input" docs), Step 3 frees `own_lane` in the very same row Step 4 then
        // needs to open a *new*, genuinely different lane for the second parent `g`. Before the
        // fix, Step 4's `allocate_lane` call happily reused `own_lane`'s own just-freed slot,
        // producing a nonsensical `Elbow { from_lane: N, to_lane: N }` self-loop instead of a
        // real elbow into a distinct lane.
        //
        // `f` renders first (lane 0, still expecting `base`); `m` merges `f` (already seen - out
        // of order relative to `base`) and `g` (not yet seen) - `m`'s own lane is brand new
        // (nothing was expecting `m`), and freed again immediately by Step 3 since `f` is
        // already in `seen`.
        let commits: Vec<(&str, Vec<&str>)> = vec![
            ("f", vec!["base"]),
            ("m", vec!["f", "g"]),
            ("base", vec![]),
            ("g", vec![]),
        ];
        let layout = layout_lanes(&commits);
        let m_row = &layout[1];
        assert_eq!(
            m_row.elbows.len(),
            1,
            "m must get exactly one real elbow: {:?}",
            m_row.elbows
        );
        let elbow = m_row.elbows[0];
        assert_ne!(
            elbow.from_lane, elbow.to_lane,
            "m's second parent must open a genuinely different lane, never a self-loop back \
             onto m's own just-freed lane: {elbow:?}"
        );
        assert_eq!(
            elbow.kind,
            ElbowKind::Diverging,
            "m's second parent is a real, new merge elbow: {elbow:?}"
        );
    }

    #[test]
    fn lanes_are_recycled_after_they_free() {
        // Two unrelated, non-overlapping branch-and_merge sequences one after another; the
        // second must reuse lane 1 rather than opening lane 2, since lane 1 is free again by
        // then (spec: "lane 5 carries two different branches in the reference history").
        let commits: Vec<(&str, Vec<&str>)> = vec![
            ("m2", vec!["a2", "b2"]),
            ("a2", vec!["base2"]),
            ("b2", vec!["base2"]),
            ("base2", vec!["m1"]),
            ("m1", vec!["a1", "b1"]),
            ("a1", vec!["base1"]),
            ("b1", vec!["base1"]),
            ("base1", vec![]),
        ];
        let layout = layout_lanes(&commits);
        let max_lane = layout.iter().map(|row| row.lane).max().unwrap_or(0);
        assert!(
            max_lane <= 1,
            "lane 1 must be recycled for the second branch, not grown to lane 2 (max lane was {max_lane})"
        );
    }

    #[test]
    fn root_commit_with_no_parents_ends_its_lane() {
        let commits: Vec<(&str, Vec<&str>)> = vec![("only", vec![])];
        let layout = layout_lanes(&commits);
        assert_eq!(layout[0].lane, 0);
        assert!(layout[0]
            .segments
            .iter()
            .any(|s| s.lane == 0 && s.ends_here && s.starts_here));
    }

    // ---- build_graph: real gix walk over a real repo ----

    #[test]
    fn build_graph_walks_linear_history_newest_first() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "first");
        commit(repo.path(), "a.txt", "2", "second");
        commit(repo.path(), "a.txt", "3", "third");

        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        let subjects: Vec<&str> = graph
            .rows
            .iter()
            .map(|row| row.commit.subject.as_str())
            .collect();
        assert_eq!(subjects, vec!["third", "second", "first"]);
        assert!(matches!(graph.rows[0].dot_kind, DotKind::Head));
        assert_eq!(graph.lane_count, 1);
    }

    #[test]
    fn build_graph_reports_a_real_merge_commit_and_branch_chip() {
        let repo = seed_empty_repo();
        commit_at(repo.path(), "a.txt", "1", "base", 1_700_000_000);
        git(repo.path(), &["checkout", "-b", "feature"]);
        commit_at(repo.path(), "b.txt", "1", "feature work", 1_700_000_100);
        git(repo.path(), &["checkout", "main"]);
        merge_at(
            repo.path(),
            "feature",
            "Merge branch 'feature'",
            1_700_000_200,
        );

        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        let merge_row = graph
            .rows
            .iter()
            .find(|row| row.commit.subject == "Merge branch 'feature'")
            .expect("merge row present");
        assert!(merge_row.commit.is_merge());
        assert_eq!(merge_row.commit.parent_ids.len(), 2);
        assert!(
            !merge_row.elbows.is_empty(),
            "merge row should open/connect a lane"
        );

        let feature_row = graph
            .rows
            .iter()
            .find(|row| row.commit.subject == "feature work")
            .expect("feature row present");
        assert!(
            feature_row.lane > 0,
            "feature commit should not render on the trunk lane"
        );
    }

    #[test]
    fn a_merge_commit_that_is_also_head_still_gets_the_merge_dot_kind() {
        // A real, reproduced bug: `dot_kind` checked `head_id` before `commit.is_merge()`, so
        // a commit that is honestly both (this repository's own `master` tip is one right now)
        // lost its merge styling entirely - the single most merge-shaped row in the graph
        // rendered as a plain `Head` dot instead. `HEAD` is already shown separately and
        // unambiguously via the branch's own ref-chip styling, so prioritizing `Merge` here
        // loses no real information.
        let repo = seed_empty_repo();
        commit_at(repo.path(), "a.txt", "1", "base", 1_700_000_000);
        git(repo.path(), &["checkout", "-b", "feature"]);
        commit_at(repo.path(), "b.txt", "1", "feature work", 1_700_000_100);
        git(repo.path(), &["checkout", "main"]);
        merge_at(
            repo.path(),
            "feature",
            "Merge branch 'feature'",
            1_700_000_200,
        );

        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        let head_row = &graph.rows[0];
        assert_eq!(head_row.commit.subject, "Merge branch 'feature'");
        assert!(
            head_row.commit.is_merge(),
            "sanity check: HEAD really is a merge commit"
        );
        assert!(
            matches!(head_row.dot_kind, DotKind::Merge),
            "a commit that is both HEAD and a real merge must still render as Merge, not Head - \
             got {:?}",
            head_row.dot_kind
        );
    }

    #[test]
    fn build_graph_current_scope_is_first_parent_only() {
        let repo = seed_empty_repo();
        commit_at(repo.path(), "a.txt", "1", "base", 1_700_000_000);
        git(repo.path(), &["checkout", "-b", "feature"]);
        commit_at(repo.path(), "b.txt", "1", "feature work", 1_700_000_100);
        git(repo.path(), &["checkout", "main"]);
        merge_at(
            repo.path(),
            "feature",
            "Merge branch 'feature'",
            1_700_000_200,
        );

        let graph = build_graph(repo.path(), GraphScope::Current, 0).expect("build_graph");
        assert!(
            !graph
                .rows
                .iter()
                .any(|row| row.commit.subject == "feature work"),
            "Current scope must not include the feature branch's own commits"
        );
    }

    #[test]
    fn build_graph_ref_chips_reflect_real_branches() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "base");
        git(repo.path(), &["branch", "topic"]);

        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        let row = &graph.rows[0];
        let names: Vec<&str> = row
            .commit
            .refs
            .iter()
            .map(|chip| chip.name.as_str())
            .collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"topic"));
        let main_chip = row
            .commit
            .refs
            .iter()
            .find(|chip| chip.name == "main")
            .expect("main chip");
        assert!(main_chip.is_head);
        assert_eq!(main_chip.kind, RefKind::LocalBranch);
    }

    #[test]
    fn build_graph_on_empty_repo_returns_no_rows() {
        let repo = seed_empty_repo();
        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        assert!(graph.rows.is_empty());
    }

    #[test]
    fn build_graph_worktrees_scope_is_limited_to_checked_out_branches() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "base");
        git(repo.path(), &["checkout", "-b", "no-worktree-branch"]);
        commit(repo.path(), "b.txt", "1", "only on no-worktree-branch");
        git(repo.path(), &["checkout", "main"]);

        let graph = build_graph(repo.path(), GraphScope::Worktrees, 0).expect("build_graph");
        assert!(
            !graph
                .rows
                .iter()
                .any(|row| row.commit.subject == "only on no-worktree-branch"),
            "a branch with no worktree must not appear under the Worktrees scope"
        );
        assert!(graph.rows.iter().any(|row| row.commit.subject == "base"));
    }

    #[test]
    fn ahead_behind_against_upstream_is_none_without_a_configured_upstream() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "base");
        let result =
            ahead_behind_against_upstream(repo.path()).expect("ahead_behind_against_upstream");
        assert_eq!(result, None);
    }

    #[test]
    fn ahead_behind_against_upstream_counts_real_divergence() {
        let remote = seed_empty_repo();
        commit(remote.path(), "a.txt", "1", "base");

        let local_dir = TempDir::new().expect("tempdir");
        git(
            local_dir.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(
            local_dir.path(),
            &["config", "user.email", "test@example.com"],
        );
        git(local_dir.path(), &["config", "user.name", "Test User"]);
        commit(local_dir.path(), "b.txt", "1", "local only");

        let result = ahead_behind_against_upstream(local_dir.path())
            .expect("ahead_behind_against_upstream")
            .expect("an upstream is configured by clone");
        assert_eq!(result.ahead, 1);
        assert_eq!(result.behind, 0);
    }

    #[test]
    fn commits_already_on_upstream_is_none_without_a_configured_upstream() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "base");
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        let result =
            commits_already_on_upstream(repo.path(), &[head]).expect("commits_already_on_upstream");
        assert_eq!(result, None);
    }

    #[test]
    fn commits_already_on_upstream_distinguishes_pushed_from_local_only_commits() {
        let remote = seed_empty_repo();
        commit(remote.path(), "a.txt", "1", "base");

        let local_dir = TempDir::new().expect("tempdir");
        git(
            local_dir.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(
            local_dir.path(),
            &["config", "user.email", "test@example.com"],
        );
        git(local_dir.path(), &["config", "user.name", "Test User"]);
        let pushed = git_output(local_dir.path(), &["rev-parse", "HEAD"]);
        commit(local_dir.path(), "b.txt", "1", "local only");
        let local_only = git_output(local_dir.path(), &["rev-parse", "HEAD"]);

        let result =
            commits_already_on_upstream(local_dir.path(), &[pushed.clone(), local_only.clone()])
                .expect("commits_already_on_upstream")
                .expect("an upstream is configured by clone");
        assert_eq!(
            result,
            vec![pushed],
            "only the commit that already exists on the remote's tip should be reported - not \
             the local-only one"
        );
        assert!(!result.contains(&local_only));
    }

    #[test]
    fn build_graph_row_order_is_stable_even_with_tied_commit_timestamps() {
        // Regression test for a real failure mode found while building this module: with
        // multiple tips (here, `main` and the still-extant `feature` branch after a merge) fed
        // into the walk, commits created back-to-back within the test process can share a
        // timestamp. The old time-sorted walk could then hand back a parent before one of its
        // own children; the topological walk must instead keep the order sound (and
        // `layout_lanes`'s defense-in-depth must keep even unsound input from panicking).
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "base");
        git(repo.path(), &["checkout", "-b", "feature"]);
        commit(repo.path(), "b.txt", "1", "feature work");
        git(repo.path(), &["checkout", "main"]);
        git(
            repo.path(),
            &[
                "merge",
                "--no-ff",
                "feature",
                "-m",
                "Merge branch 'feature'",
            ],
        );

        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        assert_eq!(graph.rows.len(), 3);
        let subjects: std::collections::HashSet<&str> = graph
            .rows
            .iter()
            .map(|row| row.commit.subject.as_str())
            .collect();
        assert!(subjects.contains("base"));
        assert!(subjects.contains("feature work"));
        assert!(subjects.contains("Merge branch 'feature'"));
    }

    #[test]
    fn graph_walk_is_prefix_stable_across_caps() {
        let repo = seed_empty_repo();
        commit_at(repo.path(), "a.txt", "1", "base", 1_700_000_000);
        git(repo.path(), &["checkout", "-b", "feature"]);
        commit_at(repo.path(), "b.txt", "1", "feature 1", 1_700_000_100);
        commit_at(repo.path(), "b.txt", "2", "feature 2", 1_700_000_200);
        git(repo.path(), &["checkout", "main"]);
        commit_at(repo.path(), "a.txt", "2", "main 1", 1_700_000_300);
        merge_at(repo.path(), "feature", "Merge feature", 1_700_000_400);
        git(repo.path(), &["checkout", "-b", "side"]);
        commit_at(repo.path(), "c.txt", "1", "side 1", 1_700_000_500);
        git(repo.path(), &["checkout", "main"]);
        commit_at(repo.path(), "a.txt", "3", "main 2", 1_700_000_600);

        let full = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        assert!(
            !full.truncated && full.rows.len() == 7,
            "precondition: the whole branchy history must fit uncapped, got {} rows",
            full.rows.len()
        );

        for cap in 1..full.rows.len() {
            let capped = build_graph(repo.path(), GraphScope::All, cap).expect("build_graph");
            assert!(
                capped.truncated,
                "a cap below the real history length must report a truncated walk"
            );
            assert_eq!(
                capped.rows.len(),
                cap,
                "a capped walk must load exactly `max_commits` rows"
            );
            assert_eq!(
                capped.rows,
                full.rows[..cap],
                "cap {cap} must be an element-identical prefix of the uncapped walk - same \
                 commits, same lanes, same segments and elbows - or the graph tab's index-keyed \
                 selection/menu/scroll would land on a different commit after a load-more"
            );
        }
    }

    /// The structural soundness the whole lane renderer stands on, checked edge by edge: no
    /// parent row ever above one of its children, every row's own lane continuing downward
    /// exactly when the commit has a parent to continue to, and every extra parent of a merge
    /// getting its own diverging elbow. The violation of these is precisely the reported "on
    /// some screens the lines seem disconnected" bug - see the module docs' "Walk order"
    /// section for how a time-sorted walk used to violate all three.
    fn assert_every_parent_edge_is_drawn(graph: &Graph) {
        let row_index_by_id: std::collections::HashMap<&str, usize> = graph
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.commit.id.as_str(), index))
            .collect();
        for (index, row) in graph.rows.iter().enumerate() {
            for parent in &row.commit.parent_ids {
                if let Some(&parent_index) = row_index_by_id.get(parent.as_str()) {
                    assert!(
                        parent_index > index,
                        "row {index} ({:?}): its parent {parent} was emitted above it (row \
                         {parent_index}) - the walk handed back a parent before its child",
                        row.commit.subject,
                    );
                }
            }
            let own = row
                .lane_segments
                .iter()
                .find(|segment| segment.lane == row.lane)
                .unwrap_or_else(|| panic!("row {index} has no segment for its own lane"));
            assert_eq!(
                own.ends_here,
                row.commit.parent_ids.is_empty(),
                "row {index} ({:?}): its own lane must end exactly when the commit has no \
                 parent at all - an `ends_here` stub on a commit that has one is a line \
                 stopping dead above a parent it should reach, the reported disconnect",
                row.commit.subject,
            );
            let diverging = row
                .elbows
                .iter()
                .filter(|elbow| elbow.kind == ElbowKind::Diverging)
                .count();
            assert_eq!(
                diverging,
                row.commit.parent_ids.len().saturating_sub(1),
                "row {index} ({:?}): every extra parent of a merge must get its own diverging \
                 elbow - a dropped one paints a merge with a branch line silently missing",
                row.commit.subject,
            );
        }
    }

    #[test]
    fn a_clock_skewed_branch_still_connects_to_its_parent() {
        let repo = seed_empty_repo();
        commit_at(repo.path(), "a.txt", "1", "base", 1_700_000_300);
        git(repo.path(), &["branch", "skewed"]);
        commit_at(repo.path(), "a.txt", "2", "main 1", 1_700_000_400);
        commit_at(repo.path(), "a.txt", "3", "main 2", 1_700_000_500);
        git(repo.path(), &["checkout", "skewed"]);
        commit_at(repo.path(), "b.txt", "1", "skewed work", 1_700_000_200);
        git(repo.path(), &["checkout", "main"]);

        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        assert_eq!(graph.rows.len(), 4);
        assert_every_parent_edge_is_drawn(&graph);

        // And the connected shape, stated exactly: the skewed commit sits *above* its parent
        // despite its older timestamp, on its own lane, and the parent's row carries the real
        // converging elbow that joins that lane back onto the trunk's dot.
        let subjects: Vec<&str> = graph
            .rows
            .iter()
            .map(|row| row.commit.subject.as_str())
            .collect();
        assert_eq!(subjects, vec!["main 2", "main 1", "skewed work", "base"]);
        let skewed = &graph.rows[2];
        let base = &graph.rows[3];
        assert_ne!(skewed.lane, base.lane);
        assert!(
            base.elbows.iter().any(|elbow| {
                elbow.kind == ElbowKind::Converging
                    && elbow.from_lane == skewed.lane
                    && elbow.to_lane == base.lane
            }),
            "the shared parent's row must join the skewed branch's lane onto its own dot with \
             a real converging elbow, got {:?}",
            base.elbows,
        );
    }

    #[test]
    fn same_second_parent_child_pairs_stay_topologically_ordered() {
        let repo = seed_empty_repo();
        let t = 1_700_000_000;
        commit_at(repo.path(), "a.txt", "1", "base", t);
        git(repo.path(), &["checkout", "-b", "side"]);
        commit_at(repo.path(), "b.txt", "1", "side work", t);
        git(repo.path(), &["checkout", "main"]);
        commit_at(repo.path(), "a.txt", "2", "main work", t);
        merge_at(repo.path(), "side", "merge side", t);
        git(repo.path(), &["checkout", "-b", "late"]);
        commit_at(repo.path(), "c.txt", "1", "late work", t);
        git(repo.path(), &["checkout", "main"]);

        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        assert_eq!(graph.rows.len(), 5);
        assert_every_parent_edge_is_drawn(&graph);
    }

    #[test]
    fn graph_walk_prefix_stays_stable_across_caps_on_a_skewed_history() {
        let repo = seed_empty_repo();
        commit_at(repo.path(), "a.txt", "1", "base", 1_700_000_300);
        git(repo.path(), &["branch", "skewed"]);
        commit_at(repo.path(), "a.txt", "2", "main 1", 1_700_000_400);
        commit_at(repo.path(), "a.txt", "3", "main 2", 1_700_000_500);
        git(repo.path(), &["checkout", "skewed"]);
        commit_at(repo.path(), "b.txt", "1", "skewed work", 1_700_000_200);
        git(repo.path(), &["checkout", "main"]);

        let full = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        assert!(!full.truncated && full.rows.len() == 4);
        for cap in 1..full.rows.len() {
            let capped = build_graph(repo.path(), GraphScope::All, cap).expect("build_graph");
            assert!(capped.truncated);
            assert_eq!(
                capped.rows,
                full.rows[..cap],
                "cap {cap} must stay an element-identical prefix of the uncapped walk even on \
                 a history whose time order and topological order disagree"
            );
        }
    }

    #[test]
    fn commit_changed_files_reports_real_add_modify_delete() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "first");
        fs::write(repo.path().join("a.txt"), "2").expect("write");
        fs::write(repo.path().join("b.txt"), "new").expect("write");
        git(repo.path(), &["add", "a.txt", "b.txt"]);
        git(repo.path(), &["commit", "-m", "second"]);
        fs::remove_file(repo.path().join("a.txt")).expect("remove");
        git(repo.path(), &["add", "a.txt"]);
        git(repo.path(), &["commit", "-m", "third"]);

        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        let second = graph
            .rows
            .iter()
            .find(|row| row.commit.subject == "second")
            .expect("second row");
        let changes =
            commit_changed_files(repo.path(), &second.commit.id).expect("commit_changed_files");
        assert_eq!(changes.len(), 2);
        assert!(changes
            .iter()
            .any(|c| c.path == Path::new("a.txt")
                && c.status == crate::diff::FileChangeStatus::Modified));
        assert!(changes
            .iter()
            .any(|c| c.path == Path::new("b.txt")
                && c.status == crate::diff::FileChangeStatus::Added));

        let third = graph
            .rows
            .iter()
            .find(|row| row.commit.subject == "third")
            .expect("third row");
        let changes =
            commit_changed_files(repo.path(), &third.commit.id).expect("commit_changed_files");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].status, crate::diff::FileChangeStatus::Deleted);
    }

    #[test]
    fn commit_changed_files_rejects_a_non_hex_commit_id() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "first");
        let err = commit_changed_files(repo.path(), "not-a-sha; rm -rf /")
            .expect_err("must reject a non-hex commit id");
        assert!(matches!(err, Error::WorktreeIo(_)));
    }

    #[test]
    fn commit_changed_files_on_a_merge_is_honestly_empty() {
        let repo = seed_empty_repo();
        commit_at(repo.path(), "a.txt", "1", "base", 1_700_000_000);
        git(repo.path(), &["checkout", "-b", "feature"]);
        commit_at(repo.path(), "b.txt", "1", "feature work", 1_700_000_100);
        git(repo.path(), &["checkout", "main"]);
        merge_at(
            repo.path(),
            "feature",
            "Merge branch 'feature'",
            1_700_000_200,
        );

        let graph = build_graph(repo.path(), GraphScope::All, 0).expect("build_graph");
        let merge_row = graph
            .rows
            .iter()
            .find(|row| row.commit.subject == "Merge branch 'feature'")
            .expect("merge row");
        let changes =
            commit_changed_files(repo.path(), &merge_row.commit.id).expect("commit_changed_files");
        assert!(
            changes.is_empty(),
            "a merge commit's default diff is empty, per git's own behavior - not a guess"
        );
    }
}
