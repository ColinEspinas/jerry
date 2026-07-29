//! Real path / `path:line[:col]` link detection inside terminal output text
//! (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s 2026-07-29 entry, change 5: "Paths and
//! `file:line` references render as links"). A pure, GPUI-free scanner over plain `&str` -
//! `crate::terminal_pane::render_row` is the one real call site, splitting a grid row's
//! already-style-merged runs further at whatever spans this reports.
//!
//! ## Scope: biased toward real compiler/test-runner output, not a general path/URL grammar
//!
//! This is deliberately not an exhaustive filesystem-path or URL grammar - the design's own
//! spec calls out exactly the class of text this needs to recognize: real `cargo build`/
//! `cargo test` output (`src/main.rs:42:10`, `  --> src/lib.rs:12:5`), the same output this
//! very terminal renders constantly. Two shapes are recognized:
//!
//! 1. **A path containing at least one `/`, whose final segment ends in a real, known
//!    extension** ([`KNOWN_BARE_EXTENSIONS`] - the same allow-list shape 2 uses), optionally
//!    followed by `:<line>` or `:<line>:<col>` - covers every real compiler/test-runner
//!    reference (`src/upload/multipart.rs:66`, `tests/upload.rs:88`) and a plain multi-segment
//!    path with no line number (`rg`/`git status --short` output: `benches/query.rs`). This
//!    shape used to accept *any* extension here rather than reusing the allow-list, which is a
//!    real, checker-found false-positive class all its own (`12.5/100.0 MB downloaded`, `the
//!    answer is 42/7.0 approx`, `git@github.com:foo/bar.git`'s own `foo/bar.git`, `curl
//!    http://.../8080/api/v1/health.json`'s own `8080/api/v1/health.json`) - each of those
//!    "ends in a `/`-containing thing followed by `.<anything>`" just as much as a real path
//!    does, so the extension itself has to be a real, known one to tell them apart.
//! 2. **A bare filename with no `/` at all, whose extension is a real, well-known one**
//!    ([`KNOWN_BARE_EXTENSIONS`]) - covers `git status --short` output for a repo-root file
//!    (`Cargo.toml`, with no directory component at all). A slash-free path needs this second,
//!    narrower rule specifically to avoid linking ordinary prose that merely contains a dot
//!    (`e.g.`, `v0.14.0`, `p = 0.05`) - none of those end in a real recognized extension, so
//!    none of them are ever matched by it.
//!
//! Both shapes require a real word boundary (`\b`) immediately after the matched extension -
//! without it, `[\w-]+\.(?:known)` only needs known extension text as a *prefix* of whatever
//! follows the dot, so `github.com` would partial-match `github.c` (`"c"` is a real, known bare
//! extension) and leave `om` dangling as unmatched plain text. The boundary rejects that: `c`
//! immediately followed by the word character `o` is not a boundary, so the match is refused
//! there and (since no other known extension is a prefix of `com` either) `github.com` never
//! matches at all - which is what makes a real `git@github.com:foo/bar.git` SSH remote URL safe
//! to leave un-linked without also removing `c`/`h` from the allow-list.
//!
//! A real URL (`scheme://...`) is actively rejected, not merely "out of scope": matched by a
//! throwaway, uncaptured alternative ahead of the two real path shapes above
//! (`(?:[a-z][a-z0-9+.-]*://\S+)`) that consumes the whole URL before either path alternative
//! ever gets a chance to match a substring of it - real cargo/npm/git output prints a bare
//! `https://...` URL constantly (`see https://doc.rust-lang.org/cargo/reference/manifest.html
//! for more`), and the naive version of shape 1's `/?` leading alternation used to latch onto
//! the second slash of `://` itself, turning a URL into a bogus absolute-path link
//! (`/doc.rust-lang.org/cargo/reference/manifest.html` - a real, reproduced false positive, not
//! theoretical). [`find_links`] relies on this alternative never populating the `path` capture
//! group to discard it automatically, rather than a second post-hoc filter.
//!
//! Deliberately not attempted: `~`-relative paths (`~/.jerry/wt/index-scan` - no real
//! extension on the last segment to anchor on), Windows-style `\`-separated paths, and a
//! general URL grammar (a URL is actively excluded from matching at all, per the above - never
//! itself treated as a navigable link).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

/// Extensions recognized for a final path segment, whether or not it has a leading `/`-having
/// prefix - i.e. both a **slash-free** bare filename (the module docs' shape 2) and the final
/// segment of a `/`-containing path (shape 1) are restricted to this same list - deliberately a
/// curated, real allow-list rather than "any `word.word` shape", which would also match ordinary
/// prose (`e.g.`, `i.e.`, `v1.2`) and real, checker-found false positives that merely *look*
/// path-shaped (`12.5/100.0`, `42/7.0`, `foo/bar.git`). Kept small and focused on real source/
/// config files this app's own target audience (developers reading `cargo`/`git`/`rg` output)
/// actually sees at a bare repo root - not an exhaustive extension registry.
const KNOWN_BARE_EXTENSIONS: &[&str] = &[
    "rs", "toml", "json", "md", "txt", "lock", "yml", "yaml", "py", "js", "jsx", "ts", "tsx", "go",
    "rb", "c", "h", "hpp", "cpp", "cc", "java", "kt", "swift", "sh", "bash", "zsh", "css", "html",
    "htm", "sql", "xml", "ini", "cfg", "log",
];

/// `None` only if the hand-written pattern below somehow fails to compile - which would be a
/// real bug in this source file, since the pattern is built entirely from this module's own
/// literal, never-user-supplied pieces. This project's own hard rule ("no `.unwrap()`/
/// `.expect()` outside `#[cfg(test)]`") means that can't be a panic even though it's
/// practically unreachable: logged loudly (`log::error!`) and every real call site
/// ([`find_links`]) degrades to "no links detected" rather than crashing the whole app over a
/// terminal-link-rendering nicety. Every one of this module's own real tests already exercises
/// this path indirectly (a `None` here would make every positive test below fail), so a
/// regression here can't ship silently.
fn link_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            let bare_extensions = KNOWN_BARE_EXTENSIONS.join("|");
            let pattern = format!(
                r"(?:[a-z][a-z0-9+.-]*://\S+)|(?P<path>/?(?:[\w.@+-]+/)+[\w.@+-]+\.(?:{bare_extensions})\b|[\w-]+\.(?:{bare_extensions})\b)(?::(?P<line>[0-9]+)(?::(?P<col>[0-9]+))?)?"
            );
            match Regex::new(&pattern) {
                Ok(regex) => Some(regex),
                Err(err) => {
                    log::error!("terminal_links: hand-written pattern failed to compile: {err}");
                    None
                }
            }
        })
        .as_ref()
}

/// One detected link inside a line of real terminal text - char (not byte) offsets into the
/// original `&str`, so a caller indexing a `Vec<GridCell>` (one cell per *character*, per
/// `crate::terminal_grid`'s own docs) can slice directly without a second byte-to-char
/// conversion of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMatch {
    /// Char offset of the first character of the link (inclusive).
    pub start: usize,
    /// Char offset just past the last character of the link (exclusive).
    pub end: usize,
    /// The path exactly as written in the source text (relative or absolute, not yet resolved
    /// against any cwd - see [`resolve`]).
    pub path: String,
    /// The 1-based line number, if the text included a `:<line>` suffix.
    pub line: Option<u32>,
    /// The 1-based column number, if the text included a `:<line>:<col>` suffix.
    pub column: Option<u32>,
}

/// Scans one line of real terminal text for path/`path:line[:col]` references - see the
/// module docs for exactly what shapes are recognized. Matches are returned left-to-right,
/// non-overlapping (the underlying regex's own `find_iter`/`captures_iter` guarantee).
pub fn find_links(text: &str) -> Vec<LinkMatch> {
    let Some(regex) = link_regex() else {
        return Vec::new();
    };
    regex
        .captures_iter(text)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            let path = caps.name("path")?.as_str().to_string();
            let line = caps
                .name("line")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                // A captured `:0` (e.g. `foo.rs:0`) is filtered to `None` here rather than
                // passed through as a real line number - `crate::root::code_surface::
                // AdeApp::navigate_to_definition`'s own `one_based_line` parameter name makes
                // the contract explicit: `0` is not a valid 1-based line, and letting it flow
                // through would hand that call a nonsensical target instead of honestly
                // reporting "no line was given".
                .filter(|&line| line != 0);
            let column = caps
                .name("col")
                .and_then(|m| m.as_str().parse::<u32>().ok());
            // `whole.start()`/`.end()` are byte offsets (`regex`'s own documented contract) -
            // converted to char offsets here so callers never need to redo this conversion
            // against a `Vec<GridCell>` indexed one entry per character (a real, not
            // theoretical, distinction: a row can contain multi-byte glyphs before a link,
            // e.g. `  ↳ tests/upload.rs:88:` - see this module's own tests).
            let start = text[..whole.start()].chars().count();
            let end = text[..whole.end()].chars().count();
            Some(LinkMatch {
                start,
                end,
                path,
                line,
                column,
            })
        })
        .collect()
}

/// Lexically collapses `..`/`.` path components - no filesystem access at all (no
/// `Path::canonicalize()`, which resolves symlinks and requires the path to genuinely exist via
/// a real, blocking `stat` syscall - `crate::root::code_surface`'s own `render_file_view` doc
/// comment on its one `canonicalize()`-per-render-call already treats a single call like that as
/// a real cost worth caching against; [`resolve`] runs once per *detected link*, on every
/// rendered terminal row, every `crate::terminal_pane::POLL_INTERVAL` (~33ms) poll tick, which
/// would make a filesystem-touching normalization here a genuinely worse per-frame cost for a
/// purely textual concern - collapsing a literal `..` in a string). `Path::components()` already
/// classifies `..`/`.` for us; a `..` with nowhere left to pop (already at the real filesystem
/// root, or before any component has been pushed for a relative path) is simply dropped, the
/// same real "can't go above the top" behavior a shell's own `cd ..` has at `/`.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Resolves a detected link's real path against `cwd` - an absolute `path` (starts with `/`)
/// is returned as-is (`PathBuf::join`'s own documented behavior: joining an absolute path onto
/// anything simply replaces it), a relative one is joined onto `cwd`. Mirrors
/// `crate::root::code_surface::navigate_to_definition`'s own already-established "resolve
/// against the real session/worktree root, not the app's own process cwd" contract - callers
/// pass the session's own `TerminalSpec::cwd`, never `std::env::current_dir()`.
///
/// ## Deliberate: `..` is normalized away, and a resolved path landing outside `cwd` is allowed
///
/// [`normalize_lexically`] collapses any `..`/`.` segments so the returned path is a real,
/// honest filesystem path, never one that still literally contains `..` - real terminal output
/// genuinely does this (`no such file or directory: ../../../etc/shadow.conf`). This
/// deliberately does **not** then also reject a normalized path that lands outside `cwd`/the
/// worktree: this is a read-only file *viewer*, not a write path, and
/// `crate::root::code_surface::AdeApp::open_terminal_link`'s own real `path.is_file()` existence
/// check (the fix for the separate "no existence check at all" bug this same audit found) is
/// what actually stops a bogus/malicious-looking path from ever opening a tab - a path that
/// escapes `cwd` but genuinely exists is no more openable-and-therefore-dangerous here than one
/// that doesn't escape it, since nothing this function's result ever reaches performs a write.
/// A real terminal session legitimately prints real paths outside its own worktree constantly (a
/// `$CARGO_HOME` registry source file inside a compiler error, another checked-out worktree, a
/// global config file) - refusing to ever resolve those would make this viewer strictly less
/// useful than a real terminal emulator's own "click to open" for no corresponding safety gain.
pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    normalize_lexically(&cwd.join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_real_cargo_style_path_line_col_reference() {
        let matches = find_links("src/main.rs:42:10");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "src/main.rs");
        assert_eq!(matches[0].line, Some(42));
        assert_eq!(matches[0].column, Some(10));
        assert_eq!(matches[0].start, 0);
        assert_eq!(matches[0].end, "src/main.rs:42:10".chars().count());
    }

    #[test]
    fn detects_a_real_cargo_diagnostic_arrow_line_with_a_leading_prefix() {
        let text = "  --> src/lib.rs:12:5";
        let matches = find_links(text);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "src/lib.rs");
        assert_eq!(matches[0].line, Some(12));
        assert_eq!(matches[0].column, Some(5));
        // The link must start exactly where the real path begins, not at the arrow.
        assert_eq!(matches[0].start, text.find("src/lib.rs").unwrap());
    }

    #[test]
    fn a_line_number_with_no_column_is_a_real_partial_match_not_a_whole_line_style() {
        let matches = find_links("thread panicked at tests/upload.rs:88");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "tests/upload.rs");
        assert_eq!(matches[0].line, Some(88));
        assert_eq!(matches[0].column, None);
    }

    /// The CHANGELOG's own example: `  ↳ tests/upload.rs:88:` must link only the path portion,
    /// not the whole line - and the trailing bare `:` (no digits after it) must not be
    /// swallowed into the match. `↳` is a real multi-byte UTF-8 character, so this also proves
    /// the reported offsets are char offsets, not raw byte offsets.
    #[test]
    fn links_only_the_path_span_inside_an_otherwise_plain_line() {
        let text = "  \u{21b3} tests/upload.rs:88:";
        let matches = find_links(text);
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert_eq!(m.path, "tests/upload.rs");
        assert_eq!(m.line, Some(88));
        let chars: Vec<char> = text.chars().collect();
        let linked: String = chars[m.start..m.end].iter().collect();
        assert_eq!(linked, "tests/upload.rs:88");
        // Everything after the match (the bare trailing colon) is real plain suffix text, not
        // consumed into the link.
        let suffix: String = chars[m.end..].iter().collect();
        assert_eq!(suffix, ":");
    }

    #[test]
    fn a_bare_repo_root_filename_with_a_known_extension_is_a_real_link() {
        // `git status --short` output for a file with no directory component at all.
        let matches = find_links(" M Cargo.toml");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "Cargo.toml");
        assert_eq!(matches[0].line, None);
    }

    #[test]
    fn a_multi_segment_path_with_no_line_number_is_still_a_real_link() {
        // `rg`/`git status --short`-style output: a real path, no `:line` suffix at all.
        let matches = find_links(" A src/db/query_builder.rs");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "src/db/query_builder.rs");
        assert_eq!(matches[0].line, None);
    }

    #[test]
    fn ordinary_prose_with_dots_but_no_real_extension_is_never_linked() {
        for line in [
            "e.g. this failed",
            "see v0.14.0 for details",
            "p = 0.00 < 0.05",
            "left: 5242880",
            "scan/baseline           time:   [41.203 ms 41.878 ms 42.611 ms]",
            "Compiling jerry-db v0.14.0 (~/.jerry/wt/index-scan)",
        ] {
            assert!(
                find_links(line).is_empty(),
                "expected no link in {line:?}, got {:?}",
                find_links(line)
            );
        }
    }

    #[test]
    fn multiple_links_on_one_line_are_all_detected_left_to_right() {
        let matches = find_links("see src/a.rs:1 and src/b.rs:2 both");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].path, "src/a.rs");
        assert_eq!(matches[0].line, Some(1));
        assert_eq!(matches[1].path, "src/b.rs");
        assert_eq!(matches[1].line, Some(2));
        assert!(matches[0].end <= matches[1].start);
    }

    /// A real backtrace frame (`at src/main.rs:142:9`) is structurally just another
    /// `path:line:col` occurrence - the same scanner covers it with no special-casing, which
    /// is exactly what lets `crate::terminal_pane::render_row` handle real panic output "for
    /// free" once link detection is wired into it once.
    #[test]
    fn a_real_backtrace_frame_is_detected_like_any_other_path_line_col_reference() {
        let matches = find_links("   3: my_crate::do_thing\n             at src/main.rs:142:9");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "src/main.rs");
        assert_eq!(matches[0].line, Some(142));
        assert_eq!(matches[0].column, Some(9));
    }

    #[test]
    fn resolve_joins_a_relative_path_onto_the_given_cwd() {
        let cwd = Path::new("/home/colin/wt/feature");
        assert_eq!(
            resolve(cwd, "src/main.rs"),
            PathBuf::from("/home/colin/wt/feature/src/main.rs")
        );
    }

    /// Deliberate, documented choice (see [`resolve`]'s own "Deliberate" doc section): an
    /// absolute path resolves to itself even when it names something outside `cwd`/the
    /// worktree - this function is not the real gate against a bogus link opening a tab
    /// (`crate::root::code_surface::AdeApp::open_terminal_link`'s own real `path.is_file()`
    /// check is), so it has no reason to special-case "outside cwd" as any less valid than
    /// "inside cwd".
    #[test]
    fn resolve_leaves_an_absolute_path_untouched() {
        let cwd = Path::new("/home/colin/wt/feature");
        assert_eq!(resolve(cwd, "/etc/hosts"), PathBuf::from("/etc/hosts"));
    }

    /// The audit's own real repro: `no such file or directory: ../../../etc/shadow.conf` in
    /// terminal output must resolve to a real, literal-`..`-free path - and, per [`resolve`]'s
    /// own documented choice, is allowed to land outside `cwd` rather than being silently
    /// rejected or left containing a raw `..`.
    #[test]
    fn resolve_normalizes_dot_dot_segments_and_deliberately_allows_escaping_cwd() {
        let cwd = Path::new("/home/colin/wt/feature");
        assert_eq!(
            resolve(cwd, "../../../etc/shadow.conf"),
            PathBuf::from("/home/etc/shadow.conf"),
            "`..` segments must be collapsed into a real path with no literal `..` left in it"
        );
    }

    #[test]
    fn resolve_normalizes_a_dot_dot_that_would_otherwise_go_above_the_root() {
        let cwd = Path::new("/home");
        assert_eq!(
            resolve(cwd, "../../../etc/passwd"),
            PathBuf::from("/etc/passwd"),
            "a `..` with nowhere left to pop (already at the real filesystem root) must be \
             dropped, not underflow into something nonsensical"
        );
    }

    /// The exact live repro the audit reproduced: a real cargo help-text line containing a bare
    /// `https://` URL, mod-clicked, used to resolve to a nonexistent absolute path
    /// (`/doc.rust-lang.org/cargo/reference/manifest.html`) because the old regex's leading `/?`
    /// alternation latched onto the second slash of `://`. A URL must never be detected as a
    /// link at all - cargo/npm/git/rustc output prints one constantly.
    #[test]
    fn a_url_in_real_cargo_output_is_never_detected_as_a_link() {
        let text = "see https://doc.rust-lang.org/cargo/reference/manifest.html for more";
        assert!(
            find_links(text).is_empty(),
            "expected no link in {text:?}, got {:?}",
            find_links(text)
        );
    }

    /// Four further real false positives the audit found by probing the production regex
    /// directly against realistic terminal output - none of these are real paths.
    #[test]
    fn realistic_non_path_output_never_produces_a_false_link() {
        for line in [
            // A `git clone`/`git remote -v` SSH-style remote: before the fix, this matched a
            // fake `github.c` (`"c"` is a real, known bare extension) *and* a fake `foo/bar.git`.
            "$ git clone git@github.com:foo/bar.git",
            // A download-progress line: before the fix, the slash-having alternation accepted
            // *any* extension, so `12.5/100.0` looked exactly like a real `path/file.ext`.
            "12.5/100.0 MB downloaded",
            "the answer is 42/7.0 approx",
            // A URL whose host:port looks like a real path once the scheme/host prefix is
            // ignored (`8080/api/v1/health.json`) - covered by the same URL rejection as the
            // `doc.rust-lang.org` case above, from a different real shape (`curl`, not `see`).
            "curl http://127.0.0.1:8080/api/v1/health.json",
        ] {
            assert!(
                find_links(line).is_empty(),
                "expected no link in {line:?}, got {:?}",
                find_links(line)
            );
        }
    }

    /// `foo.rs:0` is a real, if unusual, shape terminal output could contain - `0` must never
    /// flow through as a real line number (`crate::root::code_surface::AdeApp::
    /// navigate_to_definition`'s own `one_based_line` parameter name is explicit about why).
    #[test]
    fn a_captured_line_number_of_zero_is_treated_as_no_line() {
        let matches = find_links("foo.rs:0");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "foo.rs");
        assert_eq!(
            matches[0].line, None,
            "a captured `:0` must never pass through as a real one-based line target"
        );
    }
}
