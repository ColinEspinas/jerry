//! The pure, GPUI-free content search behind the right panel's Search tab: the compiled matcher
//! the three modifier buttons produce, the worktree walk, the two-level result tree, and the real
//! in-place replace.
//!
//! Everything here is a plain function over plain data with no `Window`, no `Context` and no app
//! state, which is the split every feature folder in this crate uses (`crate::sidebar::file_tree`
//! vs `crate::sidebar::render`). The panel itself is `crate::search::render`.
//!
//! ## One matcher, three buttons
//!
//! `Aa` (match case), `ab` (whole word) and `.*` (regex) are not three code paths - they are three
//! inputs to one [`Matcher`], which is always a real `regex::Regex`. A literal query is
//! `regex::escape`d first; whole-word wraps the whole thing in `\b(?:...)\b`. That is one place
//! for the "leftmost, non-overlapping" semantics every editor's find has, rather than a
//! hand-rolled substring scan for two of the three states and a regex for the third - which is
//! exactly how the two would silently drift apart on overlapping matches.
//!
//! The one behaviour that genuinely differs by mode is what the *replacement* string means: in
//! regex mode `$1` is a real capture reference (what a user typing a regex expects, and what VS
//! Code does), and in literal mode it is a literal dollar sign. See [`Matcher::replace_all`].
//!
//! ## Bounded, because a worktree is not bounded
//!
//! A `target/`- or `node_modules`-style build/dependency directory can hold hundreds of thousands
//! of files, and (see "Scoped to real content" below) this walk no longer even opens one -
//! but the caps here are kept regardless, as real defense in depth against whatever the active
//! worktree really contains once it *is* real, trackable content: an enormous monorepo, or a
//! generated-and-committed directory not on the explicit exclude list. Four real limits,
//! each reported honestly rather than silently applied: [`MAX_MATCHES`], [`MAX_SCANNED_FILES`],
//! [`MAX_FILE_BYTES`] and a binary-file check. [`SearchOutcome::truncated`] is what the panel's
//! count row turns into a real truncation notice - the issue's own "results cap with an honest
//! truncation notice".
//!
//! ## Parallel, because a directory walk is not one file
//!
//! A live report (GitHub issue #162's own follow-up) found a real, unbounded-looking query
//! latency: typing into the query field could take "a very long time" to answer on a checkout of
//! merely "dozens to hundreds of files" - measured directly against a 415-file, 14MB fixture at
//! **582ms** for a single-threaded, one-file-at-a-time walk (a query that does not hit either
//! cap, so every candidate file is really read and scanned). [`search_worktree`]'s own read+scan
//! step (the expensive part - `fs::read` plus a real `regex::Regex` pass, not the directory
//! listing, which is a cheap `stat` per entry) now runs across [`SEARCH_SCAN_BATCH`]-sized
//! batches of candidates on `rayon`'s global thread pool, folding each batch's results back into
//! [`SearchOutcome`] **sequentially and in the original, sorted order** - so [`MAX_MATCHES`]/
//! [`MAX_SCANNED_FILES`] still stop the walk at exactly the same file/line the old sequential loop
//! would have (`worktree_tests::the_result_cap_stops_the_search_and_says_so_rather_than_returning_a_silent_prefix`
//! still pins the same tight `<= MAX_MATCHES + 1` bound), and a re-run never reorders rows the
//! user was reading mid-keystroke. Batching (rather than handing the whole candidate list to
//! `rayon` at once) is what keeps [`MAX_SCANNED_FILES`]'s own point intact: without it, a
//! `target/`-sized checkout would still pay to read and scan every file the cap exists to avoid
//! reading in the first place, just on more threads at once.
//!
//! ## Cancelled, because a superseded search is not a finished one
//!
//! `crate::search::render::AdeApp::start_search` already discarded a slow search's *result* once
//! a newer one answered first (its own generation guard), but the slow search itself kept running
//! to completion on the background executor regardless - burning a real CPU thread competing with
//! the query that superseded it, on every keystroke of a fast typist against a large-enough
//! worktree. [`search_worktree_cancellable`] is the real fix: `is_stale` is polled once per batch
//! (cheap - a single atomic load) and a `true` answer stops the walk immediately, returning
//! whatever partial [`SearchOutcome`] has accumulated so far. The caller never looks at that
//! outcome (its own generation guard already discards it), so this is a pure CPU-saving early
//! exit, not a correctness-affecting one. [`search_worktree`] is the non-cancellable convenience
//! wrapper every existing (and every non-panel) caller keeps using.
//!
//! ## Scoped to real content, not to every byte on disk
//!
//! A second live report, against a real repository rather than a fixture: "it is very slow still
//! ... it can take 10 seconds", plus its own corroborating clue - "the perf of the search seems
//! faster after the first query". Both are explained by the same real defect, and neither the
//! rayon batching above nor the cancellation below touch it, because it sits one step earlier:
//! candidate *discovery*. The walk that fed both of those fixes candidates skipped only `.git` -
//! every other directory, including a `.gitignore`d build or dependency directory, was opened and
//! `stat`-ed like any other. Measured directly against this application's *own* checkout: 125,242
//! files on disk, of which 99,250 (79%) sit under its own `target/` - 31 GB of Rust build output.
//! [`MAX_SCANNED_FILES`] (20,000) is smaller than `target/` alone, so the old walk hit that cap,
//! and reported itself truncated, entirely inside `target/` - before a single real source file
//! was ever read.
//!
//! The first fix here (#387/#388) made [`wt_core::worktree_files::list_worktree_files`] (`git
//! ls-files --cached --others --exclude-standard`) the *sole* candidate source, which fixed the
//! real regression but, as a direct, immediate live pushback put it: "Wait what you made the
//! search respect gitignore? This should have nothing to do with git?" - correct: a search whose
//! entire notion of "don't walk into `target/`" was defined by `.gitignore`, and which stopped
//! excluding anything at all outside a real git worktree, was never the intended fix.
//!
//! GitHub issue #394 reworked this into the two-layer model [`crate::search::exclude`]'s own
//! module docs describe in full (VS Code's own `files.exclude`/`search.exclude` +
//! `search.useIgnoreFiles` split, collapsed into Jerry's one combined always-on list since there
//! is no separate file-explorer walk to share the distinction with): [`collect_candidate_files`]
//! below now runs [`crate::search::exclude::collect_files_excluding`] - a real, rayon-parallelized
//! filesystem walk pruned by [`SearchRequest::search_excludes`] - as the one, always-on,
//! git-independent primary discovery mechanism, and layers `list_worktree_files` on top of it as
//! an *additive*, independently toggleable filter
//! ([`crate::settings::store::EditorSettings::respect_gitignore`], default `true`) rather than the
//! sole file source. Measured directly against this repository, in both toggle states: see
//! [`collect_candidate_files`]'s own docs.
//!
//! GitHub issue #401 made that pruning list itself real, persisted, user-editable settings state
//! ([`crate::settings::store::EditorSettings::search_excludes`]) rather than the compiled-in
//! [`crate::search::exclude::DEFAULT_EXCLUDES`] constant alone - see [`SearchRequest::
//! search_excludes`]'s own docs for exactly what's threaded through, and `crate::search::exclude`'s
//! own "GitHub issue #401" docs for the full replace-vs-additive design decision.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::search::exclude;
use crate::search::glob::GlobList;
use wt_core::worktree_files::list_worktree_files;

/// The largest file this search will read. The same ceiling
/// `crate::code_surface::code_view::MAX_FILE_BYTES` puts on opening a file in the editor, for the
/// same reason: past it, the thing on disk is not source you are searching, and reading it costs
/// more than any hit in it is worth.
pub const MAX_FILE_BYTES: u64 = crate::code_surface::code_view::MAX_FILE_BYTES as u64;

/// How many matches one search collects before it stops and reports itself truncated. High enough
/// that a real question about a real worktree is answered in full; low enough that a one-character
/// query against a large checkout cannot build a multi-million-row tree the panel would then have
/// to render.
pub const MAX_MATCHES: usize = 2_000;

/// How many files one search will open before it stops and reports itself truncated. A separate
/// bound from [`MAX_MATCHES`] because the expensive case is the *opposite* one: a long, specific
/// query that matches almost nothing still reads every file in the worktree.
pub const MAX_SCANNED_FILES: usize = 20_000;

/// How much of a file is sniffed for a NUL byte before it is called binary and skipped.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// How many candidate files [`search_worktree_cancellable`] hands to `rayon` at once. Large
/// enough that a "dozens to hundreds of files" checkout (the live report this exists for) is one
/// batch, so it gets full parallelism; small enough that [`MAX_SCANNED_FILES`]/`is_stale` are both
/// re-checked often enough on a checkout big enough to need them - see this module's own "Bounded"
/// and "Cancelled" docs.
const SEARCH_SCAN_BATCH: usize = 128;

/// The three modifier buttons in the query row, as real state
/// (`REVISION-2026-08-14.md` §5: "`Aa` / `ab` / `.*` modifier buttons").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchOptions {
    /// `Aa` - off means the search is case-insensitive.
    pub match_case: bool,
    /// `ab` - the query must sit on word boundaries.
    pub whole_word: bool,
    /// `.*` - the query is a regular expression rather than literal text.
    pub regex: bool,
}

/// A compiled query. Construct with [`Matcher::compile`]; an invalid regex is a real, reportable
/// state ([`MatcherError`]) rather than a silent fallback to a literal search, which would answer
/// a question the user did not ask.
#[derive(Debug, Clone)]
pub struct Matcher {
    regex: regex::Regex,
    /// Kept so [`Self::replace_all`] can tell a real regex replacement (where `$1` is a capture
    /// reference) from a literal one (where it is a dollar sign) - see this module's own docs.
    regex_mode: bool,
}

/// Why a query could not be compiled - shown verbatim under the query row, since a regex error
/// message is the only thing that can tell the user which character is the problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatcherError(pub String);

impl std::fmt::Display for MatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Matcher {
    /// Compiles `query` under `options`. Returns `Ok(None)` for a query that is empty once
    /// trimmed of nothing at all - i.e. genuinely `""` - which is the panel's "not searched yet"
    /// state and not an error.
    ///
    /// A query of pure whitespace is a **real** query: a user searching for `"    "` (an
    /// indentation width) means it, and treating it as empty would silently refuse a legitimate
    /// search. Only a genuinely empty string is the idle state.
    pub fn compile(query: &str, options: SearchOptions) -> Result<Option<Self>, MatcherError> {
        if query.is_empty() {
            return Ok(None);
        }
        let body = if options.regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        // `\b(?:...)\b` rather than `\b...\b`: without the non-capturing group, a top-level
        // alternation in regex mode (`foo|bar`) would bind as `(\bfoo)|(bar\b)` and quietly stop
        // meaning "whole word".
        let pattern = if options.whole_word {
            format!(r"\b(?:{body})\b")
        } else {
            body
        };
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!options.match_case)
            .build()
            .map_err(|error| MatcherError(first_line_of(&error.to_string())))?;
        Ok(Some(Matcher {
            regex,
            regex_mode: options.regex,
        }))
    }

    /// Every non-overlapping match in `line`, as byte ranges into it.
    ///
    /// Zero-length matches are dropped rather than reported. They are reachable the moment the
    /// user types `.*` or `a?` into a regex query, and a zero-width "match" is not something the
    /// tree can highlight, the count can honestly total, or replace can act on - it would produce
    /// one result row per character in the worktree.
    pub fn find_in_line(&self, line: &str) -> Vec<Range<usize>> {
        self.regex
            .find_iter(line)
            .filter(|found| found.start() != found.end())
            .map(|found| found.range())
            .collect()
    }

    /// `text` with every match replaced, plus how many were replaced.
    ///
    /// In regex mode `replacement` is a real template: `$1` / `${name}` expand to captures, which
    /// is what a user who just typed a regex means by it. In literal mode it is inserted verbatim
    /// (`regex::NoExpand`), so replacing with `$5.00` writes `$5.00` rather than an empty capture.
    ///
    /// Zero-length matches are skipped here for the same reason [`Self::find_in_line`] drops them,
    /// and by the same code path - so the count this returns is always exactly the count the tree
    /// showed.
    pub fn replace_all(&self, text: &str, replacement: &str) -> (String, usize) {
        let mut out = String::with_capacity(text.len());
        let mut last = 0usize;
        let mut count = 0usize;
        for captures in self.regex.captures_iter(text) {
            // `regex`'s own contract: group 0 is the whole match and is always present on a
            // yielded `Captures` - but this stays a graceful skip rather than an `expect()`, since
            // nothing here depends on that invariant holding to stay safe.
            let Some(whole) = captures.get(0) else {
                continue;
            };
            if whole.start() == whole.end() {
                continue;
            }
            out.push_str(&text[last..whole.start()]);
            if self.regex_mode {
                captures.expand(replacement, &mut out);
            } else {
                out.push_str(replacement);
            }
            last = whole.end();
            count += 1;
        }
        out.push_str(&text[last..]);
        (out, count)
    }
}

/// A regex error renders as several lines with a caret diagram, which is more than a one-line
/// notice under a 28px field can show. The first line is the sentence that names the problem.
fn first_line_of(message: &str) -> String {
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(message)
        .trim()
        .to_string()
}

/// The two path-filter fields, resolved into the one question the walk asks per file.
///
/// The asymmetry between them is deliberate and is the whole reason this is a type rather than
/// two `GlobList`s: an **empty include** means "no include filter", i.e. every path is a
/// candidate, while an empty exclude means "exclude nothing". Both read as "the field is blank, so
/// it is not filtering", but they are opposite defaults on a bare `GlobList::matches`.
#[derive(Debug, Clone, Default)]
pub struct PathFilter {
    include: GlobList,
    exclude: GlobList,
}

impl PathFilter {
    pub fn new(include: &str, exclude: &str) -> Self {
        PathFilter {
            include: GlobList::parse(include),
            exclude: GlobList::parse(exclude),
        }
    }

    /// Whether a worktree-relative, `/`-separated path survives both fields.
    pub fn allows(&self, relative: &str) -> bool {
        if !self.include.is_empty() && !self.include.matches(relative) {
            return false;
        }
        !self.exclude.matches(relative)
    }
}

/// One matched line, and every hit on it.
///
/// The whole line is kept rather than a pre-trimmed display string: the panel's own left-elision
/// rule ([`elide_around`]) is a *rendering* decision that depends on which hit a row is showing,
/// and baking it in here would make the same data unusable for replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMatch {
    /// 1-based, as every editor and every `grep` reports it.
    pub line_number: usize,
    pub text: String,
    /// Byte ranges into [`Self::text`], in order, non-overlapping.
    pub ranges: Vec<Range<usize>>,
}

/// One file's whole contribution to the tree - the file row plus the match rows under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatches {
    /// Absolute, so a click can open it without re-deriving the worktree root.
    pub path: PathBuf,
    /// Worktree-relative and `/`-separated - what the file row prints and what the path filter
    /// matched.
    pub relative: String,
    pub lines: Vec<LineMatch>,
}

impl FileMatches {
    /// Hits in this file, counting **matches** rather than lines - two hits on one line are two
    /// results, which is what the tree draws and what Replace all would act on.
    pub fn match_count(&self) -> usize {
        self.lines.iter().map(|line| line.ranges.len()).sum()
    }

    /// The file's own name, as the file row's leading label.
    pub fn file_name(&self) -> &str {
        match self.relative.rsplit_once('/') {
            Some((_, name)) => name,
            None => self.relative.as_str(),
        }
    }

    /// The dimmed directory beside it, with its trailing `/` - `""` for a root-level file.
    pub fn directory(&self) -> &str {
        match self.relative.rfind('/') {
            Some(index) => &self.relative[..=index],
            None => "",
        }
    }
}

/// A completed search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchOutcome {
    pub files: Vec<FileMatches>,
    /// Total hits across every file - the `N results` half of the count row.
    pub total_matches: usize,
    /// A real limit was reached ([`MAX_MATCHES`] or [`MAX_SCANNED_FILES`]), so this is a prefix of
    /// the truth and the panel must say so.
    pub truncated: bool,
    /// How many files were really opened and scanned - the number the truncation notice quotes.
    pub scanned_files: usize,
}

/// Everything one search run needs. A struct rather than five parameters because the panel builds
/// it from five separate widgets and three of them are strings that would be transposable.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    /// The worktree being searched - `STAGE-A-CHANGELOG.md` §4u: "scoped to the active worktree
    /// like the tree beside it".
    pub root: PathBuf,
    pub matcher: Matcher,
    pub filter: PathFilter,
    /// Layer one's own real, persisted pattern list - `crate::settings::store::EditorSettings::
    /// search_excludes` (GitHub issue #401), compiled by [`collect_candidate_files`] via
    /// [`crate::search::exclude::exclude_list_from`] into the [`crate::search::glob::GlobList`]
    /// [`crate::search::exclude::collect_files_excluding`] prunes the walk against, before layer
    /// two (below) is ever consulted. `crate::search::render::AdeApp::start_search` is the one
    /// real call site that populates this, straight from the live setting, so an edit in Settings
    /// takes effect on the very next query with no restart needed - the same guarantee
    /// `respect_gitignore` already makes.
    pub search_excludes: Vec<String>,
    /// The gitignore layer's own toggle - `crate::settings::store::EditorSettings::
    /// respect_gitignore`, `true` by default. See [`collect_candidate_files`]'s own docs for
    /// exactly how this composes with the always-on explicit exclude list underneath it.
    pub respect_gitignore: bool,
}

/// Runs a real content search over `request.root`.
///
/// Blocking, by design: the caller runs it on `gpui::BackgroundExecutor` exactly as
/// `crate::sidebar::file_tree::build_file_tree` is run (see `crate::search::render::AdeApp::
/// start_search`). Errors reading an individual file or directory are skipped rather than
/// aborting - one unreadable folder must never blank a whole result tree - which is the same call
/// `build_file_tree`'s own walk makes.
///
/// The non-cancellable convenience wrapper: every caller that isn't the panel's own debounced,
/// generation-guarded search (every test in this module included) has nothing to cancel *for* -
/// see [`search_worktree_cancellable`]'s own docs for the one caller that does.
pub fn search_worktree(request: &SearchRequest) -> SearchOutcome {
    search_worktree_cancellable(request, &|| false)
}

/// [`search_worktree`], plus the real fix for a live, reported "typing is slow" defect: `is_stale`
/// is polled between batches, and answering `true` stops the walk immediately rather than running
/// it to completion only to have the result discarded - see this module's own "Cancelled" docs.
///
/// `is_stale` is called from this function's own thread only (never from inside a `rayon` worker),
/// so it needs no `Send`/`Sync` bound of its own.
pub fn search_worktree_cancellable(
    request: &SearchRequest,
    is_stale: &dyn Fn() -> bool,
) -> SearchOutcome {
    let mut outcome = SearchOutcome::default();
    let mut candidates = collect_candidate_files(
        &request.root,
        &request.search_excludes,
        request.respect_gitignore,
        &mut outcome,
    );
    // A stable, predictable order: the tree is read top to bottom and a search re-run after a
    // keystroke must not shuffle rows the user was reading. Neither `git ls-files`' own order nor
    // `read_dir`'s is defined to be that.
    candidates.sort();

    // The include/exclude filter is real path-string work, not file IO, so it stays sequential -
    // parallelizing it would not meaningfully speed up the walk, and doing it here keeps every
    // batch below holding only real candidates to read.
    let filtered: Vec<(PathBuf, String)> = candidates
        .into_iter()
        .filter(|(_path, relative)| request.filter.allows(relative))
        .collect();

    for batch in filtered.chunks(SEARCH_SCAN_BATCH) {
        if outcome.truncated || is_stale() {
            break;
        }
        // The expensive part - `fs::read` plus a real `regex::Regex` pass per file - run across
        // `rayon`'s global thread pool. Each file's own full match set is computed independently
        // and without regard to `MAX_MATCHES` (there is no running total to check from inside a
        // parallel closure); the cap is enforced afterwards, sequentially, in the fold loop below
        // - the same place [`MAX_SCANNED_FILES`] already was, so both caps still stop the walk at
        // exactly the file/line the old single-threaded loop would have.
        let scanned: Vec<Option<Vec<LineMatch>>> = batch
            .par_iter()
            .map(|(path, _relative)| scan_file(path, &request.matcher))
            .collect();

        for ((path, relative), lines) in batch.iter().zip(scanned) {
            if outcome.truncated {
                break;
            }
            if outcome.scanned_files >= MAX_SCANNED_FILES {
                outcome.truncated = true;
                break;
            }
            let Some(all_lines) = lines else {
                // Not searchable at all (binary/oversized/unreadable/non-UTF-8) - never counted
                // as scanned, matching `read_searchable`'s own callers before this change.
                continue;
            };
            outcome.scanned_files += 1;
            let mut kept = Vec::with_capacity(all_lines.len());
            for line in all_lines {
                outcome.total_matches += line.ranges.len();
                kept.push(line);
                if outcome.total_matches >= MAX_MATCHES {
                    outcome.truncated = true;
                    break;
                }
            }
            if !kept.is_empty() {
                outcome.files.push(FileMatches {
                    path: path.clone(),
                    relative: relative.clone(),
                    lines: kept,
                });
            }
        }
    }
    outcome
}

/// `path`'s full, uncapped match set, or `None` when [`read_searchable`] says it is not
/// searchable at all. Pure per-file work - no shared state, no cap enforcement - so it is safe to
/// call from any `rayon` worker thread; [`search_worktree_cancellable`]'s own fold loop is where
/// [`MAX_MATCHES`] actually gets enforced, against the real running total this function has no
/// access to.
fn scan_file(path: &Path, matcher: &Matcher) -> Option<Vec<LineMatch>> {
    let content = read_searchable(path)?;
    Some(
        content
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                let ranges = matcher.find_in_line(line);
                if ranges.is_empty() {
                    return None;
                }
                Some(LineMatch {
                    line_number: index + 1,
                    text: line.to_string(),
                    ranges,
                })
            })
            .collect(),
    )
}

/// Every real candidate file under `request.root`, as an absolute path paired with its
/// worktree-relative, `/`-separated form - the two-layer model [`crate::search::exclude`]'s own
/// module docs describe in full, and what GitHub issue #394 reworked #387/#388's gitignore-only
/// fix into.
///
/// **Layer one, always on:** [`crate::search::exclude::collect_files_excluding`] - a real,
/// `rayon`-parallelized filesystem walk pruned by `search_excludes`, compiled via
/// [`crate::search::exclude::exclude_list_from`] (GitHub issue #401's real, persisted
/// `crate::settings::store::EditorSettings::search_excludes` - `target`, `node_modules`, `.git`,
/// and a handful of other common build/dependency directory names *by default*, and genuinely
/// user-editable from there) *before* a directory is ever opened. This is the primary discovery
/// mechanism now, not a fallback: it runs identically whether or not `root` is a real git
/// worktree, which is the literal answer to the live pushback this issue exists for - "this should
/// have nothing to do with git".
///
/// **Layer two, toggleable:** when `request.respect_gitignore` is `true` (the default -
/// `crate::settings::store::EditorSettings::respect_gitignore`), [`list_worktree_files`] (`git
/// ls-files --cached --others --exclude-standard`, #388's own real fix) is additionally consulted
/// and the layer-one candidates are narrowed to exactly the paths it lists - i.e. gitignored files
/// are removed **on top of** whatever layer one already pruned, never instead of it. Outside a
/// real git worktree (or wherever `git` itself can't run), [`list_worktree_files`] returns `Err`
/// and this layer is a no-op: layer one alone is already a complete, honest answer there. When
/// `respect_gitignore` is `false`, this layer is skipped entirely - a search deliberately scoped
/// independently of git, which is the literal behaviour the pushback asked for.
///
/// Measured directly against this repository's own real checkout (31 GB `target/` + 28 GB
/// `.shared-target/`, neither one walked by layer one regardless of `respect_gitignore`): both
/// toggle states stay fast - see this crate's own `PR #394` real functional verification for exact
/// numbers, not asserted here as a flaky wall-clock unit test.
fn collect_candidate_files(
    root: &Path,
    search_excludes: &[String],
    respect_gitignore: bool,
    outcome: &mut SearchOutcome,
) -> Vec<(PathBuf, String)> {
    let excludes = exclude::exclude_list_from(search_excludes);
    let (mut candidates, walk_truncated) =
        exclude::collect_files_excluding(root, &excludes, MAX_SCANNED_FILES);
    if walk_truncated {
        outcome.truncated = true;
    }

    if respect_gitignore {
        if let Ok(listing) = list_worktree_files(root) {
            if listing.truncated {
                outcome.truncated = true;
            }
            let allowed: HashSet<String> = listing.files.into_iter().collect();
            candidates.retain(|(_path, relative)| allowed.contains(relative));
        }
        // `Err` (not a real git worktree, or `git` couldn't be run at all): layer one's own
        // output is already the complete, honest answer - see this function's own docs.
    }

    candidates
}

/// `path`'s text, or `None` when it is not something to search: too large
/// ([`MAX_FILE_BYTES`]), binary (a NUL byte in the first [`BINARY_SNIFF_BYTES`]), not valid UTF-8,
/// or simply unreadable.
///
/// The UTF-8 check is real rather than lossy: a lossy decode would report byte offsets into a
/// string that is not what is on disk, and replace would then write those offsets back.
pub fn read_searchable(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// One file's real, on-disk replace: reads it, replaces every match, writes it back only if
/// something actually changed.
///
/// Re-reads rather than trusting the [`SearchOutcome`] the tree is showing - that outcome can be
/// seconds old and an agent may have rewritten the file since. The count returned is therefore
/// the count that really landed, which is what "report what changed" has to mean.
pub fn replace_in_file(
    path: &Path,
    matcher: &Matcher,
    replacement: &str,
) -> io::Result<ReplacedFile> {
    let Some(content) = read_searchable(path) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a searchable text file",
        ));
    };
    let (replaced, count) = matcher.replace_all(&content, replacement);
    if count == 0 || replaced == content {
        return Ok(ReplacedFile {
            path: path.to_path_buf(),
            matches: 0,
        });
    }
    fs::write(path, replaced.as_bytes())?;
    Ok(ReplacedFile {
        path: path.to_path_buf(),
        matches: count,
    })
}

/// What one file's replace really did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacedFile {
    pub path: PathBuf,
    /// Zero when the file was re-read and no longer matched - a real outcome, not a failure.
    pub matches: usize,
}

/// The whole outcome of a Replace all / per-file replace, as the panel reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaceOutcome {
    pub files_changed: usize,
    pub matches_replaced: usize,
    /// Files that were deliberately not touched because they are open in the editor with unsaved
    /// edits - see [`replace_across`]'s own docs.
    pub skipped_dirty: Vec<PathBuf>,
    /// Files whose write really failed, with the OS's own message.
    pub failed: Vec<(PathBuf, String)>,
}

/// Replaces across `files`, skipping any path in `dirty` entirely.
///
/// `dirty` is the set of files currently open in the editor with **unsaved** changes
/// (`crate::code_surface::edit_buffer::EditBuffer::is_dirty`). Writing those would silently
/// destroy edits the user has not saved: the replace is computed from what is on disk, so it
/// would write disk-content-with-substitutions over a buffer the editor still believes it owns,
/// and the editor's own next save would then write the un-replaced buffer straight back. Refusing
/// and *naming* them is the only honest option - `REVISION-2026-08-14.md` §7 rule 1's "ship the
/// affordance with the behaviour, or ship neither", applied to a partial one.
pub fn replace_across(
    files: &[PathBuf],
    matcher: &Matcher,
    replacement: &str,
    dirty: &HashSet<PathBuf>,
) -> ReplaceOutcome {
    let mut outcome = ReplaceOutcome::default();
    for path in files {
        if dirty.contains(path) {
            outcome.skipped_dirty.push(path.clone());
            continue;
        }
        match replace_in_file(path, matcher, replacement) {
            Ok(replaced) if replaced.matches > 0 => {
                outcome.files_changed += 1;
                outcome.matches_replaced += replaced.matches;
            }
            Ok(_) => {}
            Err(error) => outcome.failed.push((path.clone(), error.to_string())),
        }
    }
    outcome
}

/// How many characters of context sit before the hit on a match row before the prefix elides from
/// the left. `Jerry.dc.html`'s own `trimPre`: `s.length > 16 ? '…' + s.slice(-15)`.
pub const ELIDE_PREFIX_MAX: usize = 16;

/// The same for the tail. `Jerry.dc.html`'s own `trimPost`: `s.length > 26 ? s.slice(0, 25) + '…'`.
pub const ELIDE_SUFFIX_MAX: usize = 26;

/// Splits `line` around `range` into the three spans a match row draws, with the design's own
/// left-elision applied.
///
/// `Jerry.dc.html` states the rule and the reason verbatim: "The row is ~40 characters at 10px
/// mono. A long prefix pushes the match clean out of the box, which defeats the point of showing
/// the line at all - so the prefix elides from the LEFT and the hit stays at a fixed early column,
/// the way VS Code does it. The tail may overflow; the tail is only context."
///
/// Counts **characters**, not bytes, so an indented line of CJK or a comment with an emoji in it
/// elides at the same visual width as an ASCII one rather than three times earlier.
pub fn elide_around(line: &str, range: &Range<usize>) -> (String, String, String) {
    let before = &line[..range.start];
    let hit = &line[range.clone()];
    let after = &line[range.end..];

    let before_chars: Vec<char> = before.chars().collect();
    let prefix = if before_chars.len() > ELIDE_PREFIX_MAX {
        let tail: String = before_chars[before_chars.len() - (ELIDE_PREFIX_MAX - 1)..]
            .iter()
            .collect();
        format!("\u{2026}{tail}")
    } else {
        before.to_string()
    };

    let after_chars: Vec<char> = after.chars().collect();
    let suffix = if after_chars.len() > ELIDE_SUFFIX_MAX {
        let head: String = after_chars[..ELIDE_SUFFIX_MAX - 1].iter().collect();
        format!("{head}\u{2026}")
    } else {
        after.to_string()
    };

    (prefix, hit.to_string(), suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(query: &str, options: SearchOptions) -> Matcher {
        Matcher::compile(query, options)
            .expect("compiles")
            .expect("a non-empty query")
    }

    fn literal(query: &str) -> Matcher {
        matcher(query, SearchOptions::default())
    }

    #[test]
    fn an_empty_query_is_the_idle_state_not_an_error() {
        assert!(Matcher::compile("", SearchOptions::default())
            .expect("no error")
            .is_none());
    }

    #[test]
    fn a_whitespace_only_query_is_a_real_search() {
        let matcher = literal("    ");
        assert_eq!(
            matcher.find_in_line("    indented").len(),
            1,
            "searching for an indentation width is a real thing to want; only a genuinely empty \
             field is the not-searched-yet state"
        );
    }

    #[test]
    fn a_literal_query_never_reads_as_a_regex() {
        let matcher = literal("a.c");
        assert!(
            matcher.find_in_line("abc").is_empty(),
            "`.` must be literal"
        );
        assert_eq!(matcher.find_in_line("a.c").len(), 1);
    }

    #[test]
    fn match_case_off_is_case_insensitive_and_on_is_not() {
        let insensitive = literal("Token");
        assert_eq!(insensitive.find_in_line("refresh_token").len(), 1);
        let sensitive = matcher(
            "Token",
            SearchOptions {
                match_case: true,
                ..SearchOptions::default()
            },
        );
        assert!(sensitive.find_in_line("refresh_token").is_empty());
        assert_eq!(sensitive.find_in_line("refresh_Token").len(), 1);
    }

    #[test]
    fn whole_word_rejects_a_hit_inside_a_longer_identifier() {
        let matcher = matcher(
            "token",
            SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
        );
        assert!(
            matcher.find_in_line("refresh_token").is_empty(),
            "`_` is a word character, so `refresh_token` does not contain the whole word `token`"
        );
        assert_eq!(matcher.find_in_line("let token = 1;").len(), 1);
    }

    #[test]
    fn whole_word_over_a_regex_alternation_binds_to_the_whole_alternation() {
        // Without the `(?:...)` group this compiles to `\bfoo|bar\b`, which silently means
        // "whole-word foo, or any bar" - the exact shadowed-semantics bug the group exists for.
        let matcher = matcher(
            "foo|bar",
            SearchOptions {
                whole_word: true,
                regex: true,
                ..SearchOptions::default()
            },
        );
        assert!(matcher.find_in_line("xxbarxx").is_empty());
        assert_eq!(matcher.find_in_line("a bar b").len(), 1);
    }

    #[test]
    fn an_invalid_regex_is_a_real_reportable_error_not_a_silent_literal_fallback() {
        let error = Matcher::compile(
            "(unclosed",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
        )
        .expect_err("an unclosed group must not compile");
        assert!(!error.0.is_empty());
        assert!(
            !error.0.contains('\n'),
            "the panel shows one line under a 28px field: {}",
            error.0
        );
    }

    #[test]
    fn matches_are_leftmost_and_non_overlapping() {
        let matcher = literal("aa");
        assert_eq!(
            matcher.find_in_line("aaaa"),
            vec![0..2, 2..4],
            "two non-overlapping hits, not three overlapping ones"
        );
    }

    #[test]
    fn a_zero_width_regex_match_is_never_reported_as_a_result() {
        let matcher = matcher(
            "x*",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
        );
        assert_eq!(
            matcher.find_in_line("abc"),
            Vec::<Range<usize>>::new(),
            "`x*` matches the empty string at every offset - one result row per character is not \
             a search result"
        );
        assert_eq!(matcher.find_in_line("axxb"), vec![1..3]);
    }

    #[test]
    fn replace_in_literal_mode_never_expands_a_dollar_group() {
        let matcher = literal("PRICE");
        let (out, count) = matcher.replace_all("cost: PRICE", "$5.00");
        assert_eq!(out, "cost: $5.00");
        assert_eq!(count, 1);
    }

    #[test]
    fn replace_in_regex_mode_really_expands_captures() {
        let matcher = matcher(
            r"fn (\w+)\(",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
        );
        let (out, count) = matcher.replace_all("fn refresh(", "pub fn $1(");
        assert_eq!(out, "pub fn refresh(");
        assert_eq!(count, 1);
    }

    #[test]
    fn replace_counts_every_hit_including_several_on_one_line() {
        let matcher = literal("a");
        let (out, count) = matcher.replace_all("a b a\na", "X");
        assert_eq!(out, "X b X\nX");
        assert_eq!(count, 3);
    }

    #[test]
    fn replacing_with_the_empty_string_is_a_real_deletion() {
        let matcher = literal("_old");
        let (out, count) = matcher.replace_all("name_old = 1", "");
        assert_eq!(out, "name = 1");
        assert_eq!(count, 1);
    }

    #[test]
    fn a_path_filter_with_an_empty_include_lets_everything_through() {
        let filter = PathFilter::new("", "");
        assert!(filter.allows("src/lib.rs"));
        assert!(filter.allows("target/debug/x"));
    }

    #[test]
    fn a_real_include_narrows_and_a_real_exclude_wins_over_it() {
        let filter = PathFilter::new("src/**, tests/**", "src/generated/**");
        assert!(filter.allows("src/auth/session.rs"));
        assert!(filter.allows("tests/auth_race.rs"));
        assert!(!filter.allows("migrations/0031.sql"), "not included");
        assert!(
            !filter.allows("src/generated/api.rs"),
            "exclude must win over include, or an exclude could never narrow one"
        );
    }

    #[test]
    fn file_matches_split_a_relative_path_into_name_and_dimmed_directory() {
        let file = FileMatches {
            path: PathBuf::from("/wt/src/auth/session.rs"),
            relative: "src/auth/session.rs".to_string(),
            lines: Vec::new(),
        };
        assert_eq!(file.file_name(), "session.rs");
        assert_eq!(file.directory(), "src/auth/");

        let root_level = FileMatches {
            path: PathBuf::from("/wt/README.md"),
            relative: "README.md".to_string(),
            lines: Vec::new(),
        };
        assert_eq!(root_level.file_name(), "README.md");
        assert_eq!(root_level.directory(), "");
    }

    #[test]
    fn a_files_match_count_totals_hits_not_lines() {
        let file = FileMatches {
            path: PathBuf::from("/wt/a.rs"),
            relative: "a.rs".to_string(),
            lines: vec![
                LineMatch {
                    line_number: 1,
                    text: "a a".to_string(),
                    ranges: vec![0..1, 2..3],
                },
                LineMatch {
                    line_number: 4,
                    text: "aa".to_string(),
                    ranges: vec![0..1, 1..2],
                },
            ],
        };
        assert_eq!(
            file.match_count(),
            4,
            "two hits on one line are two results - the count row and Replace all must agree"
        );
    }

    #[test]
    fn a_long_prefix_elides_from_the_left_so_the_hit_stays_at_an_early_column() {
        let line = "                    let refresh_token = issue();";
        let range = line.find("refresh_token").expect("the hit")..;
        let range = range.start..range.start + "refresh_token".len();
        let (prefix, hit, suffix) = elide_around(line, &range);
        assert!(prefix.starts_with('\u{2026}'));
        assert_eq!(prefix.chars().count(), ELIDE_PREFIX_MAX);
        assert_eq!(hit, "refresh_token");
        assert_eq!(suffix, " = issue();");
    }

    #[test]
    fn a_short_prefix_and_tail_are_left_exactly_as_they_are() {
        let (prefix, hit, suffix) = elide_around("let a = 1;", &(4..5));
        assert_eq!(
            (prefix.as_str(), hit.as_str(), suffix.as_str()),
            ("let ", "a", " = 1;")
        );
    }

    #[test]
    fn a_long_tail_elides_from_the_right_because_the_tail_is_only_context() {
        let line = format!("x{}", "y".repeat(80));
        let (_, hit, suffix) = elide_around(&line, &(0..1));
        assert_eq!(hit, "x");
        assert_eq!(suffix.chars().count(), ELIDE_SUFFIX_MAX);
        assert!(suffix.ends_with('\u{2026}'));
    }

    #[test]
    fn elision_counts_characters_not_bytes() {
        // Twenty CJK characters is 60 bytes; a byte-counting elision would cut this three times
        // earlier than an equivalent ASCII line.
        let prefix_source = "\u{6f22}".repeat(20);
        let line = format!("{prefix_source}HIT");
        let start = prefix_source.len();
        let (prefix, hit, _) = elide_around(&line, &(start..start + 3));
        assert_eq!(hit, "HIT");
        assert_eq!(prefix.chars().count(), ELIDE_PREFIX_MAX);
    }
}

/// Real timing/cancellation regression coverage for GitHub issue #162's own live-report follow-up.
///
/// "Typing is slow" was two real, separate defects: the walk itself was single-threaded (this
/// module's own "Parallel" docs), and a superseded search kept running to completion instead of
/// stopping (this module's own "Cancelled" docs). Both are proven here against a real, on-disk,
/// 200-file worktree - `crate::search::render::panel_tests` proves the panel wires this all up
/// correctly; this proves the engine underneath it is actually fast and actually cancellable.
#[cfg(test)]
mod perf_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// A real, "dozens to hundreds of files" worktree - the live report's own wording - spread
    /// across 20 subdirectories so the walk is genuinely a directory tree, not one flat folder.
    /// Deliberately more than one [`SEARCH_SCAN_BATCH`] (200 files against a 128-file batch), so
    /// both tests below exercise a walk that really does span more than one parallel batch.
    fn many_file_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp worktree");
        let root = dir.path();
        for i in 0..200 {
            let sub = root.join(format!("src/mod{}", i % 20));
            fs::create_dir_all(&sub).expect("mkdir");
            let mut content = String::new();
            for line in 0..300 {
                content.push_str(&format!("    let value_{line} = compute(value_{line});\n"));
            }
            fs::write(sub.join(format!("file_{i}.rs")), content).expect("write");
        }
        dir
    }

    fn never_matches_request(root: &Path) -> SearchRequest {
        SearchRequest {
            root: root.to_path_buf(),
            matcher: Matcher::compile(
                "zzz_never_present_in_the_fixture_zzz",
                SearchOptions::default(),
            )
            .expect("compiles")
            .expect("a query"),
            filter: PathFilter::new("", ""),
            search_excludes: exclude::default_search_excludes(),
            respect_gitignore: true,
        }
    }

    /// A real wall-clock bound, not a micro-benchmark: loose enough (2s) that it never flakes on
    /// a slow CI runner, following the same "generous, real bound" convention
    /// `crate::lsp::client`'s own timing tests already use (`started.elapsed() < Duration::
    /// from_secs(5)`/`20)`) - but tight enough that a regression back to the pre-fix,
    /// single-threaded, one-file-at-a-time walk (measured directly at 582ms against a comparable
    /// 415-file/14MB fixture - this module's own "Parallel" docs) would still be caught.
    #[test]
    fn a_full_walk_of_a_real_multi_file_worktree_stays_well_under_a_second() {
        let dir = many_file_fixture();
        let request = never_matches_request(dir.path());
        let start = Instant::now();
        let outcome = search_worktree(&request);
        let elapsed = start.elapsed();
        assert_eq!(
            outcome.scanned_files, 200,
            "every candidate must really have been read and scanned - this bound is only honest \
             if the walk actually did the work it is being timed for"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "a real 200-file worktree walk took {elapsed:?} - see this test's own docs for why \
             2s is the right bound to assert here"
        );
    }

    /// The real mechanism behind the live report's second half: a search that is already stale
    /// before its very first batch must never scan the whole worktree anyway - it has to bail out
    /// immediately, which is exactly what frees the CPU a fast typist's next keystroke needs.
    #[test]
    fn an_already_stale_search_never_scans_a_single_file() {
        let dir = many_file_fixture();
        let request = never_matches_request(dir.path());
        let outcome = search_worktree_cancellable(&request, &|| true);
        assert_eq!(
            outcome.scanned_files, 0,
            "stale from the start - the real shape of a keystroke that supersedes a search \
             before its own debounce has even elapsed - must mean zero files read, not merely \
             fewer than the total"
        );
    }

    /// The other half: a search that goes stale *while it is running* stops at the next batch
    /// boundary rather than finishing the walk it was already partway through.
    #[test]
    fn a_search_that_goes_stale_partway_through_stops_before_scanning_everything() {
        let dir = many_file_fixture();
        let request = never_matches_request(dir.path());
        // `false` the first time this is polled (so batch 1 - 128 of the 200 files - really
        // runs), `true` every time after (so batch 2 never starts).
        let polls = AtomicUsize::new(0);
        let outcome =
            search_worktree_cancellable(&request, &|| polls.fetch_add(1, Ordering::SeqCst) >= 1);
        assert!(
            outcome.scanned_files > 0 && outcome.scanned_files < 200,
            "going stale after one batch must stop the walk before it reaches every file, not \
             merely report a smaller number afterwards once it already finished - scanned {} of \
             200",
            outcome.scanned_files
        );
    }
}

/// Real searches and real replaces over a real, multi-file worktree on disk - no in-memory stand-in
/// for the walk, the reads or the writes, because the walk's own rules (`.git`, symlinks, binary
/// files, the size cap) are exactly the part an in-memory fixture cannot exercise.
///
/// The corpus mirrors `Jerry.dc.html`'s own fixture (`refresh_token` across `src/auth/`,
/// `tests/` and `migrations/`) so the shapes asserted here are the shapes the panel was designed
/// against.
#[cfg(test)]
mod worktree_tests {
    use super::*;

    /// Writes a real worktree and returns its `TempDir` - the whole fixture in one place so every
    /// test below searches the same corpus the panel's own mock does.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp worktree");
        let root = dir.path();
        let write = |relative: &str, content: &str| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            fs::write(&path, content).expect("write");
        };
        write(
            "src/auth/session.rs",
            "use crate::store;\n\
             pub fn issue(&self) -> Token {\n    let refresh_token = self.store.issue(&sid)?;\n\
             \n    if self.refresh_token.is_expired(now) {\n        drop(refresh_token);\n    }\n}\n",
        );
        write(
            "src/auth/store.rs",
            "pub trait Store {\n    fn refresh_token(&self, sid: &SessionId) -> Option<Token>;\n}\n",
        );
        write("src/api/users.rs", "let t = auth.refresh_token(&sid)?;\n");
        write(
            "tests/auth_race.rs",
            "let a = svc.refresh_token(sid).unwrap();\nlet b = svc.refresh_token(sid).unwrap();\n",
        );
        write(
            "migrations/0031_add_refresh_lock.sql",
            "alter table sessions\n  add column refresh_token_lock boolean not null default false;\n",
        );
        write("README.md", "No hits in here.\n");
        write("Cargo.lock", "refresh_token = \"1\"\n");
        // Real `.git` bookkeeping: the one thing the walk skips unconditionally.
        write(".git/COMMIT_EDITMSG", "wip refresh_token\n");
        // A dotfile that is *not* `.git` - searchable, deliberately, see `collect_files`' docs.
        write(
            ".github/workflows/ci.yml",
            "run: cargo test refresh_token\n",
        );
        // A real binary file: a NUL byte in the sniff window.
        fs::write(root.join("logo.bin"), [0x89, 0x50, 0x00, 0x01, 0x02]).expect("write binary");
        dir
    }

    fn run(
        root: &Path,
        query: &str,
        options: SearchOptions,
        include: &str,
        exclude: &str,
    ) -> SearchOutcome {
        let matcher = Matcher::compile(query, options)
            .expect("compiles")
            .expect("a non-empty query");
        search_worktree(&SearchRequest {
            root: root.to_path_buf(),
            matcher,
            filter: PathFilter::new(include, exclude),
            search_excludes: exclude::default_search_excludes(),
            respect_gitignore: true,
        })
    }

    fn relatives(outcome: &SearchOutcome) -> Vec<&str> {
        outcome
            .files
            .iter()
            .map(|file| file.relative.as_str())
            .collect()
    }

    #[test]
    fn a_real_search_across_a_real_worktree_finds_every_file_and_every_line() {
        let dir = fixture();
        let outcome = run(
            dir.path(),
            "refresh_token",
            SearchOptions::default(),
            "",
            "",
        );

        assert_eq!(
            relatives(&outcome),
            vec![
                ".github/workflows/ci.yml",
                "Cargo.lock",
                "migrations/0031_add_refresh_lock.sql",
                "src/api/users.rs",
                "src/auth/session.rs",
                "src/auth/store.rs",
                "tests/auth_race.rs",
            ],
            "sorted, stable order - a re-run after a keystroke must not shuffle rows"
        );
        assert_eq!(
            outcome.total_matches, 10,
            "every hit, counted as a hit: three on three separate lines of session.rs, one each \
             in store.rs / users.rs / the migration / Cargo.lock / the workflow, and two in \
             auth_race.rs"
        );
        assert!(!outcome.truncated);

        let session = outcome
            .files
            .iter()
            .find(|file| file.relative == "src/auth/session.rs")
            .expect("session.rs");
        assert_eq!(
            session
                .lines
                .iter()
                .map(|line| line.line_number)
                .collect::<Vec<_>>(),
            vec![3, 5, 6],
            "real 1-based line numbers off the real file"
        );
    }

    #[test]
    fn the_git_directory_is_never_searched_but_other_dotfiles_are() {
        let dir = fixture();
        let outcome = run(
            dir.path(),
            "refresh_token",
            SearchOptions::default(),
            "",
            "",
        );
        assert!(
            !relatives(&outcome)
                .iter()
                .any(|path| path.starts_with(".git/")),
            "`.git` is this app's own bookkeeping - a hit in it is not actionable"
        );
        assert!(
            relatives(&outcome).contains(&".github/workflows/ci.yml"),
            "a search that silently cannot find text in a dotfile is an index, not a result"
        );
    }

    #[test]
    fn a_binary_file_is_skipped_rather_than_decoded() {
        let dir = fixture();
        fs::write(
            dir.path().join("blob.bin"),
            [b'r', b'e', b'f', 0x00, b'r', b'e', b'f'],
        )
        .expect("write");
        let outcome = run(dir.path(), "ref", SearchOptions::default(), "", "");
        assert!(!relatives(&outcome).contains(&"blob.bin"));
    }

    #[test]
    fn a_file_past_the_size_cap_is_skipped() {
        let dir = fixture();
        let huge = format!(
            "{}\nrefresh_token\n",
            "x".repeat(MAX_FILE_BYTES as usize + 1)
        );
        fs::write(dir.path().join("huge.rs"), huge).expect("write");
        let outcome = run(
            dir.path(),
            "refresh_token",
            SearchOptions::default(),
            "",
            "",
        );
        assert!(!relatives(&outcome).contains(&"huge.rs"));
    }

    #[test]
    fn the_include_and_exclude_fields_really_narrow_a_real_walk() {
        let dir = fixture();
        let included = run(
            dir.path(),
            "refresh_token",
            SearchOptions::default(),
            "src/**, tests/**",
            "",
        );
        assert_eq!(
            relatives(&included),
            vec![
                "src/api/users.rs",
                "src/auth/session.rs",
                "src/auth/store.rs",
                "tests/auth_race.rs",
            ]
        );

        let excluded = run(
            dir.path(),
            "refresh_token",
            SearchOptions::default(),
            "",
            "*.lock, migrations/**, .github/**",
        );
        assert_eq!(
            relatives(&excluded),
            vec![
                "src/api/users.rs",
                "src/auth/session.rs",
                "src/auth/store.rs",
                "tests/auth_race.rs",
            ],
            "the design's own `target/**, *.lock` shape, against a real tree"
        );
    }

    #[test]
    fn match_case_really_changes_a_real_result_set() {
        let dir = fixture();
        fs::write(dir.path().join("src/Cased.rs"), "Refresh_Token\n").expect("write");

        let insensitive = run(
            dir.path(),
            "Refresh_Token",
            SearchOptions::default(),
            "src/**",
            "",
        );
        assert!(insensitive.total_matches > 1);

        let sensitive = run(
            dir.path(),
            "Refresh_Token",
            SearchOptions {
                match_case: true,
                ..SearchOptions::default()
            },
            "src/**",
            "",
        );
        assert_eq!(relatives(&sensitive), vec!["src/Cased.rs"]);
        assert_eq!(sensitive.total_matches, 1);
    }

    #[test]
    fn a_real_replace_all_rewrites_every_file_on_disk_and_reports_what_changed() {
        let dir = fixture();
        let root = dir.path();
        let outcome = run(
            root,
            "refresh_token",
            SearchOptions::default(),
            "src/**",
            "",
        );
        let files: Vec<PathBuf> = outcome.files.iter().map(|file| file.path.clone()).collect();
        assert_eq!(files.len(), 3);

        let matcher = Matcher::compile("refresh_token", SearchOptions::default())
            .expect("compiles")
            .expect("a query");
        let replaced = replace_across(&files, &matcher, "rotate_token", &HashSet::new());

        assert_eq!(replaced.files_changed, 3);
        assert_eq!(replaced.matches_replaced, 5);
        assert!(replaced.skipped_dirty.is_empty());
        assert!(replaced.failed.is_empty());

        let session = fs::read_to_string(root.join("src/auth/session.rs")).expect("read back");
        assert!(
            !session.contains("refresh_token"),
            "the file on disk must really have changed: {session}"
        );
        assert!(session.contains("rotate_token"));
        assert!(
            session.contains("use crate::store;"),
            "every untouched line must survive verbatim"
        );

        // Outside the include filter, so genuinely untouched.
        let untouched = fs::read_to_string(root.join("tests/auth_race.rs")).expect("read back");
        assert!(untouched.contains("refresh_token"));

        // And the search really agrees afterwards.
        let after = run(
            root,
            "refresh_token",
            SearchOptions::default(),
            "src/**",
            "",
        );
        assert_eq!(after.total_matches, 0);
    }

    #[test]
    fn a_file_open_with_unsaved_edits_is_refused_and_named_rather_than_silently_overwritten() {
        let dir = fixture();
        let root = dir.path();
        let session = root.join("src/auth/session.rs");
        let store = root.join("src/auth/store.rs");
        let before = fs::read_to_string(&session).expect("read");

        let matcher = Matcher::compile("refresh_token", SearchOptions::default())
            .expect("compiles")
            .expect("a query");
        let dirty: HashSet<PathBuf> = [session.clone()].into_iter().collect();
        let outcome = replace_across(
            &[session.clone(), store.clone()],
            &matcher,
            "rotate",
            &dirty,
        );

        assert_eq!(outcome.skipped_dirty, vec![session.clone()]);
        assert_eq!(outcome.files_changed, 1, "the clean file is still replaced");
        assert_eq!(
            fs::read_to_string(&session).expect("read back"),
            before,
            "a file with unsaved editor changes must be byte-identical afterwards - writing it \
             would destroy edits the editor still believes it owns"
        );
        assert!(fs::read_to_string(&store)
            .expect("read back")
            .contains("rotate"));
    }

    #[test]
    fn replacing_one_file_leaves_every_other_matching_file_alone() {
        let dir = fixture();
        let root = dir.path();
        let matcher = Matcher::compile("refresh_token", SearchOptions::default())
            .expect("compiles")
            .expect("a query");

        let replaced =
            replace_in_file(&root.join("src/api/users.rs"), &matcher, "rotate").expect("replaced");
        assert_eq!(replaced.matches, 1);

        assert!(fs::read_to_string(root.join("src/api/users.rs"))
            .expect("read")
            .contains("rotate"));
        assert!(
            fs::read_to_string(root.join("src/auth/store.rs"))
                .expect("read")
                .contains("refresh_token"),
            "a per-file replace is per file"
        );
    }

    #[test]
    fn a_replace_that_re_reads_a_file_no_longer_matching_reports_zero_rather_than_writing() {
        let dir = fixture();
        let path = dir.path().join("src/api/users.rs");
        let matcher = Matcher::compile("refresh_token", SearchOptions::default())
            .expect("compiles")
            .expect("a query");
        // An agent rewrote the file between the search and the replace - the real race this
        // re-read exists for.
        fs::write(&path, "let t = auth.rotate(&sid)?;\n").expect("rewrite");
        let replaced = replace_in_file(&path, &matcher, "x").expect("no error");
        assert_eq!(replaced.matches, 0);
        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            "let t = auth.rotate(&sid)?;\n",
            "nothing matched, so nothing may be written"
        );
    }

    #[test]
    fn overlapping_candidates_are_replaced_left_to_right_without_double_counting() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("a.txt");
        fs::write(&path, "aaaa\n").expect("write");
        let matcher = Matcher::compile("aa", SearchOptions::default())
            .expect("compiles")
            .expect("a query");
        let replaced = replace_in_file(&path, &matcher, "b").expect("replaced");
        assert_eq!(
            replaced.matches, 2,
            "`aaaa` holds two non-overlapping `aa`, not three overlapping ones"
        );
        assert_eq!(fs::read_to_string(&path).expect("read back"), "bb\n");
    }

    #[test]
    fn the_result_cap_stops_the_search_and_says_so_rather_than_returning_a_silent_prefix() {
        let dir = tempfile::tempdir().expect("temp");
        let mut content = String::new();
        for _ in 0..(MAX_MATCHES + 50) {
            content.push_str("hit\n");
        }
        fs::write(dir.path().join("big.txt"), content).expect("write");
        let outcome = run(dir.path(), "hit", SearchOptions::default(), "", "");
        assert!(outcome.truncated);
        assert!(outcome.total_matches >= MAX_MATCHES);
        assert!(
            outcome.total_matches <= MAX_MATCHES + 1,
            "the cap must stop the scan, not merely be observed after it: {}",
            outcome.total_matches
        );
    }

    #[test]
    fn a_symlink_is_never_followed_so_a_loop_cannot_make_the_walk_unbounded() {
        let dir = fixture();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), dir.path().join("loop"))
                .expect("a real symlink back to the root");
            let outcome = run(
                dir.path(),
                "refresh_token",
                SearchOptions::default(),
                "",
                "",
            );
            assert!(
                !relatives(&outcome)
                    .iter()
                    .any(|path| path.starts_with("loop/")),
                "following this link would recurse forever"
            );
            assert!(!outcome.truncated);
        }
    }
}

/// Real coverage for GitHub issue #394's two-layer rework (superseding #387/#388's own
/// gitignore-only fix - see this module's own "Scoped to real content" docs). Every test above
/// this module runs against a plain, non-git [`tempfile::tempdir`], which only ever exercises the
/// always-on explicit-exclude layer ([`crate::search::exclude`]) - with no real git repo there,
/// the toggleable gitignore layer is a no-op regardless of `respect_gitignore`. This module is
/// what proves the real *composition* of the two layers inside a real git worktree: the explicit
/// list always applies and cannot be turned off, the gitignore layer is genuinely additive and
/// genuinely toggleable on top of it, and both keep holding when the fixture is exactly the shape
/// the original #387 bug was (a sizeable build directory that isn't even in `.gitignore`).
#[cfg(test)]
mod layered_exclude_tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A real git worktree with:
    /// - `target/`, a real `.gitignore`d build directory sized enough that including it would
    ///   matter, and also on [`crate::search::exclude::DEFAULT_EXCLUDES`] - excluded by *either*
    ///   layer alone.
    /// - `secret/`, also real and `.gitignore`d, but deliberately **not** on the explicit default
    ///   list - the one directory only the toggleable layer can hide, which is what makes the
    ///   two layers' composition provable rather than merely both-excluding-the-same-thing.
    /// - a real tracked file and a real untracked-but-not-ignored file, so "still finds real
    ///   content" is asserted alongside "excludes the right things", never assumed.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp worktree");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);

        let write = |relative: &str, content: &str| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            fs::write(&path, content).expect("write");
        };
        write(".gitignore", "target/\nsecret/\n");
        write(
            "src/auth/session.rs",
            "let refresh_token = store.issue(&sid)?;\n",
        );
        git(root, &["add", ".gitignore", "src/auth/session.rs"]);
        // A real `target/`-shaped build directory: several hundred files, every one of them
        // mentioning the query, so a walk that fails to exclude it would not just be slow - it
        // would visibly change the result set this test asserts on.
        for i in 0..300 {
            write(
                &format!("target/debug/deps/refresh_token-{i}.d"),
                "refresh_token\n",
            );
        }
        write("secret/refresh_token_notes.md", "refresh_token\n");
        write("src/new_untracked.rs", "refresh_token again\n");
        dir
    }

    fn run(root: &Path, query: &str, respect_gitignore: bool) -> SearchOutcome {
        let matcher = Matcher::compile(query, SearchOptions::default())
            .expect("compiles")
            .expect("a non-empty query");
        search_worktree(&SearchRequest {
            root: root.to_path_buf(),
            matcher,
            filter: PathFilter::new("", ""),
            search_excludes: exclude::default_search_excludes(),
            respect_gitignore,
        })
    }

    /// Gitignore mode on (the real default) hides both the explicitly-excluded directory and the
    /// gitignore-only one, on top of finding every real file.
    #[test]
    fn gitignore_mode_on_hides_both_the_explicit_and_the_gitignore_only_directory() {
        let dir = fixture();
        let outcome = run(dir.path(), "refresh_token", true);

        assert!(
            !outcome
                .files
                .iter()
                .any(|file| file.relative.starts_with("target/")),
            "the always-on explicit list must exclude target/: {:?}",
            relatives(&outcome)
        );
        assert!(
            !outcome
                .files
                .iter()
                .any(|file| file.relative.starts_with("secret/")),
            "the gitignore layer, additive on top, must exclude secret/: {:?}",
            relatives(&outcome)
        );
        assert!(outcome
            .files
            .iter()
            .any(|file| file.relative == "src/auth/session.rs"));
        assert!(outcome
            .files
            .iter()
            .any(|file| file.relative == "src/new_untracked.rs"));
        assert!(
            !outcome.truncated,
            "300 excluded files must not consume any of MAX_SCANNED_FILES's budget"
        );
        assert_eq!(
            outcome.scanned_files, 3,
            "only the three real files (.gitignore, session.rs, new_untracked.rs) should ever \
             have been opened and read"
        );
    }

    /// Gitignore mode off: the explicit list still applies (it cannot be turned off), but
    /// `secret/` - gitignored, and only gitignored - is found. This is the literal answer to
    /// "this should have nothing to do with git": a search deliberately scoped independently of
    /// git still excludes real build/dependency directories by name, and nothing else.
    #[test]
    fn gitignore_mode_off_still_applies_the_explicit_list_but_finds_the_gitignore_only_file() {
        let dir = fixture();
        let outcome = run(dir.path(), "refresh_token", false);

        assert!(
            !outcome
                .files
                .iter()
                .any(|file| file.relative.starts_with("target/")),
            "the explicit list is always on, regardless of respect_gitignore: {:?}",
            relatives(&outcome)
        );
        assert!(
            outcome
                .files
                .iter()
                .any(|file| file.relative == "secret/refresh_token_notes.md"),
            "a file only .gitignore would hide, and that isn't on the explicit denylist, must be \
             found with the gitignore layer off: {:?}",
            relatives(&outcome)
        );
        assert!(outcome
            .files
            .iter()
            .any(|file| file.relative == "src/auth/session.rs"));
    }

    #[test]
    fn an_explicitly_tracked_file_under_a_later_gitignore_rule_is_still_searched_with_the_layer_on()
    {
        let dir = fixture();
        let root = dir.path();
        // `session.rs` is already tracked (see `fixture`); now ignore its own directory too.
        fs::write(root.join(".gitignore"), "target/\nsecret/\nsrc/auth/\n").expect("write");

        let outcome = run(root, "refresh_token", true);
        assert!(
            outcome
                .files
                .iter()
                .any(|file| file.relative == "src/auth/session.rs"),
            "git status does not hide an explicitly tracked file just because a later ignore \
             rule would otherwise cover it, and neither should search"
        );
    }

    /// The core bug-fix guarantee (#387): a real, sizeable build/dependency directory that isn't
    /// even listed in `.gitignore` - this repository's own real `.shared-target/` before #388
    /// added it, the exact incident - must never be walked, in **either** toggle state. Proves
    /// the always-on explicit list, not `.gitignore`, is what the fix's real guarantee rests on.
    #[test]
    fn an_ungitignored_build_directory_is_excluded_in_both_toggle_states() {
        let dir = tempfile::tempdir().expect("a temp worktree");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        let write = |relative: &str, content: &str| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            fs::write(&path, content).expect("write");
        };
        write("src/lib.rs", "refresh_token\n");
        git(root, &["add", "src/lib.rs"]);
        // Deliberately no `.gitignore` at all.
        for i in 0..300 {
            write(&format!("node_modules/pkg/dist-{i}.js"), "refresh_token\n");
        }

        for respect_gitignore in [true, false] {
            let outcome = run(root, "refresh_token", respect_gitignore);
            assert!(
                !outcome
                    .files
                    .iter()
                    .any(|file| file.relative.starts_with("node_modules/")),
                "respect_gitignore={respect_gitignore}: an ungitignored build/dependency \
                 directory must still be excluded by the always-on explicit list: {:?}",
                relatives(&outcome)
            );
            assert!(
                outcome
                    .files
                    .iter()
                    .any(|file| file.relative == "src/lib.rs"),
                "respect_gitignore={respect_gitignore}"
            );
            assert!(!outcome.truncated, "respect_gitignore={respect_gitignore}");
            assert_eq!(
                outcome.scanned_files, 1,
                "respect_gitignore={respect_gitignore}"
            );
        }
    }

    fn relatives(outcome: &SearchOutcome) -> Vec<&str> {
        outcome
            .files
            .iter()
            .map(|file| file.relative.as_str())
            .collect()
    }
}

/// Real, end-to-end coverage for GitHub issue #401: `SearchRequest::search_excludes` is a real,
/// user-editable list (`crate::settings::store::EditorSettings::search_excludes`), not just the
/// compiled-in `crate::search::exclude::DEFAULT_EXCLUDES` constant. `crate::search::exclude`'s own
/// unit tests already prove `exclude_list_from`'s compilation in isolation; this module proves the
/// *whole* `search_worktree` path - request in, real result out - actually threads a caller's list
/// through rather than silently falling back to the built-in one somewhere along the way.
#[cfg(test)]
mod configurable_exclude_tests {
    use super::*;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        fs::write(&path, content).expect("write");
    }

    fn run(root: &Path, search_excludes: Vec<String>) -> SearchOutcome {
        let matcher = Matcher::compile("needle", SearchOptions::default())
            .expect("compiles")
            .expect("a non-empty query");
        search_worktree(&SearchRequest {
            root: root.to_path_buf(),
            matcher,
            filter: PathFilter::new("", ""),
            search_excludes,
            // No real git worktree in this fixture - irrelevant to what's under test here, same
            // as every plain-tempdir fixture above `layered_exclude_tests`.
            respect_gitignore: true,
        })
    }

    fn relatives(outcome: &SearchOutcome) -> Vec<&str> {
        outcome
            .files
            .iter()
            .map(|file| file.relative.as_str())
            .collect()
    }

    /// A pattern the user typed into the Settings > Editor > Search "add a pattern" row - not on
    /// `DEFAULT_EXCLUDES` at all - really prunes the walk, proving the request's own list, not the
    /// compiled-in constant, is what layer one actually excludes against.
    #[test]
    fn a_real_custom_pattern_from_settings_excludes_matching_files() {
        let dir = tempfile::tempdir().expect("a temp worktree");
        let root = dir.path();
        write(root, "src/lib.rs", "needle\n");
        write(root, "vendor/thirdparty/lib.rs", "needle\n");

        let mut patterns = exclude::default_search_excludes();
        patterns.push("vendor".to_string());
        let outcome = run(root, patterns);

        assert_eq!(
            relatives(&outcome),
            vec!["src/lib.rs"],
            "the user's own added `vendor` pattern must exclude it just like a built-in entry"
        );
    }

    /// The user removed `node_modules` from their own copy of the list in Settings (the row's
    /// remove affordance, `crate::settings::render::AdeApp::remove_search_exclude_pattern`) -
    /// a real search must now find matches inside it again, proving the removal really reaches
    /// the walk rather than only updating what Settings displays.
    #[test]
    fn removing_a_default_pattern_in_settings_re_includes_it_in_a_real_search() {
        let dir = tempfile::tempdir().expect("a temp worktree");
        let root = dir.path();
        write(root, "src/lib.rs", "needle\n");
        write(root, "node_modules/pkg/index.js", "needle\n");

        let mut patterns = exclude::default_search_excludes();
        patterns.retain(|pattern| pattern != "node_modules");
        let outcome = run(root, patterns);

        assert_eq!(
            relatives(&outcome),
            vec!["node_modules/pkg/index.js", "src/lib.rs"],
            "removing node_modules from the user's own list must really re-include it: {:?}",
            relatives(&outcome)
        );
    }

    /// The default request (what every fresh install effectively sends, before any Settings edit)
    /// still excludes `target/` - the same core #387 guarantee, now proven through the
    /// user-editable field's own real default rather than only through the old hardcoded-constant
    /// call path.
    #[test]
    fn the_real_default_search_excludes_still_excludes_target() {
        let dir = tempfile::tempdir().expect("a temp worktree");
        let root = dir.path();
        write(root, "src/lib.rs", "needle\n");
        write(root, "target/debug/deps/needle.d", "needle\n");

        let outcome = run(root, exclude::default_search_excludes());

        assert_eq!(relatives(&outcome), vec!["src/lib.rs"]);
    }
}
