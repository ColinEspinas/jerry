//! Path / `path:line[:col]` link detection inside terminal output text. A pure, GPUI-free
//! scanner over plain `&str` - `crate::terminal::pane::render_row` is the one real call site,
//! splitting a grid row's already-style-merged runs further at whatever spans this reports.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

/// Extensions recognized for a final path segment, with or without a leading `/`-having prefix -
/// deliberately a curated allow-list rather than "any `word.word` shape" (see the module docs).
/// Kept small and focused on real source/config files this app's target audience (developers
/// reading `cargo`/`git`/`rg` output) actually sees at a bare repo root.
const KNOWN_BARE_EXTENSIONS: &[&str] = &[
    "rs", "toml", "json", "md", "txt", "lock", "yml", "yaml", "py", "js", "jsx", "ts", "tsx", "go",
    "rb", "c", "h", "hpp", "cpp", "cc", "java", "kt", "swift", "sh", "bash", "zsh", "css", "html",
    "htm", "sql", "xml", "ini", "cfg", "log",
];

/// `None` only if the hand-written pattern below fails to compile - practically unreachable
/// since it's built entirely from this module's own literal pieces, but this project forbids
/// `.unwrap()`/`.expect()` outside `#[cfg(test)]`, so a failure is logged and every call site
/// ([`find_links`]) degrades to "no links detected" rather than panicking.
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

/// One detected link inside a line of terminal text - char (not byte) offsets into the original
/// `&str`, so a caller indexing a `Vec<GridCell>` (one cell per character) can slice directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMatch {
    /// Char offset of the first character of the link (inclusive).
    pub start: usize,
    /// Char offset just past the last character of the link (exclusive).
    pub end: usize,
    /// The path exactly as written in the source text, not yet resolved against any cwd - see
    /// [`resolve`].
    pub path: String,
    /// The 1-based line number, if the text included a `:<line>` suffix.
    pub line: Option<u32>,
    /// The 1-based column number, if the text included a `:<line>:<col>` suffix.
    pub column: Option<u32>,
}

/// Scans one line of terminal text for path/`path:line[:col]` references - see the module docs
/// for exactly what shapes are recognized. Matches are left-to-right, non-overlapping (the
/// underlying regex's own guarantee).
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
                // A captured `:0` (e.g. `foo.rs:0`) is not a valid 1-based line -
                // `crate::code_surface::lsp_ui::AdeApp::open_file_at_line`'s own
                // `one_based_line` parameter makes that contract explicit.
                .filter(|&line| line != 0);
            let column = caps
                .name("col")
                .and_then(|m| m.as_str().parse::<u32>().ok());
            // `whole.start()`/`.end()` are byte offsets; converted to char offsets here so
            // callers never redo this against a `Vec<GridCell>` indexed per character (a row
            // can contain multi-byte glyphs before a link, e.g. `  ↳ tests/upload.rs:88:`).
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

/// Lexically collapses `..`/`.` path components - no filesystem access (no
/// `Path::canonicalize()`, which resolves symlinks via a blocking `stat` and requires the path
/// to exist). [`resolve`] runs once per detected link on every rendered terminal row, on every
/// frame a streaming agent re-renders on, so a filesystem-touching normalization here would
/// be a real per-frame cost for a purely textual concern. (It is per *frame*, not per
/// `crate::terminal::pane::POLL_INTERVAL` poll tick as this used to claim: a tick that drains
/// bytes calls `cx.notify()`, but GPUI coalesces however many invalidations land between two
/// draws into a single frame, so at an 8ms poll interval several ticks routinely share one
/// render.)
/// `Path::components()` already classifies `..`/`.`; a `..` with nowhere left to pop is simply
/// dropped, the same as a shell's own `cd ..` at `/`.
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

/// Resolves a detected link's path against `cwd` - an absolute `path` is returned as-is
/// (`PathBuf::join`'s own documented behavior), a relative one is joined onto `cwd`. Callers
/// pass the agent's own `TerminalSpec::cwd`, never `std::env::current_dir()`.
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
        let suffix: String = chars[m.end..].iter().collect();
        assert_eq!(suffix, ":");
    }

    #[test]
    fn a_bare_repo_root_filename_with_a_known_extension_is_a_real_link() {
        let matches = find_links(" M Cargo.toml");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "Cargo.toml");
        assert_eq!(matches[0].line, None);
    }

    #[test]
    fn a_multi_segment_path_with_no_line_number_is_still_a_real_link() {
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

    #[test]
    fn resolve_leaves_an_absolute_path_untouched() {
        let cwd = Path::new("/home/colin/wt/feature");
        assert_eq!(resolve(cwd, "/etc/hosts"), PathBuf::from("/etc/hosts"));
    }

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

    #[test]
    fn a_url_in_real_cargo_output_is_never_detected_as_a_link() {
        let text = "see https://doc.rust-lang.org/cargo/reference/manifest.html for more";
        assert!(
            find_links(text).is_empty(),
            "expected no link in {text:?}, got {:?}",
            find_links(text)
        );
    }

    #[test]
    fn realistic_non_path_output_never_produces_a_false_link() {
        for line in [
            // An SSH-style git remote: before the fix, this matched a fake `github.c` (`"c"`
            // is a known bare extension) *and* a fake `foo/bar.git`.
            "$ git clone git@github.com:foo/bar.git",
            // A download-progress line: before the fix, the slash-having alternation accepted
            // *any* extension, so `12.5/100.0` looked exactly like `path/file.ext`.
            "12.5/100.0 MB downloaded",
            "the answer is 42/7.0 approx",
            // A URL whose host:port looks path-shaped once the scheme/host prefix is ignored -
            // covered by the same URL rejection as the `doc.rust-lang.org` case above.
            "curl http://127.0.0.1:8080/api/v1/health.json",
        ] {
            assert!(
                find_links(line).is_empty(),
                "expected no link in {line:?}, got {:?}",
                find_links(line)
            );
        }
    }

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
