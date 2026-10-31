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
mod link_detection_tests {
    use super::*;

    /// Every real shape of file reference this app is expected to turn into a clickable link,
    /// against the one thing that matters about each: which path, which line, which column.
    /// A backtrace frame (`at src/main.rs:142:9`) needs no special-casing - it is structurally
    /// just another `path:line:col` occurrence, which is why it sits in the same table.
    #[test]
    fn every_real_shape_of_file_reference_resolves_to_its_own_path_line_and_column() {
        for (text, path, line, column) in [
            ("src/main.rs:42:10", "src/main.rs", Some(42), Some(10)),
            ("  --> src/lib.rs:12:5", "src/lib.rs", Some(12), Some(5)),
            (
                "   3: my_crate::do_thing\n             at src/main.rs:142:9",
                "src/main.rs",
                Some(142),
                Some(9),
            ),
            (
                "thread panicked at tests/upload.rs:88",
                "tests/upload.rs",
                Some(88),
                None,
            ),
            (" M Cargo.toml", "Cargo.toml", None, None),
            (
                " A src/db/query_builder.rs",
                "src/db/query_builder.rs",
                None,
                None,
            ),
            // `:0` is a real, if unusual, shape terminal output can contain, and must never flow
            // through as a real one-based line target.
            ("foo.rs:0", "foo.rs", None, None),
        ] {
            let matches = find_links(text);
            assert_eq!(matches.len(), 1, "expected exactly one link in {text:?}");
            assert_eq!(matches[0].path, path, "path in {text:?}");
            assert_eq!(matches[0].line, line, "line in {text:?}");
            assert_eq!(matches[0].column, column, "column in {text:?}");
            // The link starts where the real path begins, not at whatever prefix precedes it.
            let start_char = text.char_indices().nth(matches[0].start).map(|(i, _)| i);
            assert_eq!(
                start_char,
                text.find(path),
                "the link span must begin at the path itself in {text:?}"
            );
        }
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
    fn multiple_links_on_one_line_are_all_detected_left_to_right() {
        let matches = find_links("see src/a.rs:1 and src/b.rs:2 both");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].path, "src/a.rs");
        assert_eq!(matches[0].line, Some(1));
        assert_eq!(matches[1].path, "src/b.rs");
        assert_eq!(matches[1].line, Some(2));
        assert!(matches[0].end <= matches[1].start);
    }

    /// The false positives found by probing the production regex against realistic terminal
    /// output. A URL is the sharpest of them: mod-clicking one used to resolve to a nonexistent
    /// absolute path (`/doc.rust-lang.org/...`), because the old regex's leading `/?`
    /// alternation latched onto the second slash of `://`.
    #[test]
    fn realistic_non_path_output_never_produces_a_false_link() {
        for line in [
            "e.g. this failed",
            "see v0.14.0 for details",
            "p = 0.00 < 0.05",
            "left: 5242880",
            "scan/baseline           time:   [41.203 ms 41.878 ms 42.611 ms]",
            "Compiling jerry-db v0.14.0 (~/.jerry/wt/index-scan)",
            "see https://doc.rust-lang.org/cargo/reference/manifest.html for more",
            // An SSH-style git remote: before the fix, this matched a fake `github.c` (`"c"`
            // is a known bare extension) *and* a fake `foo/bar.git`.
            "$ git clone git@github.com:foo/bar.git",
            // A download-progress line: before the fix, the slash-having alternation accepted
            // *any* extension, so `12.5/100.0` looked exactly like `path/file.ext`.
            "12.5/100.0 MB downloaded",
            "the answer is 42/7.0 approx",
            // A URL whose host:port looks path-shaped once the scheme/host prefix is ignored.
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
    fn resolve_produces_a_real_absolute_path_with_no_dot_dot_left_in_it() {
        for (cwd, path, expected) in [
            (
                "/home/colin/wt/feature",
                "src/main.rs",
                "/home/colin/wt/feature/src/main.rs",
            ),
            ("/home/colin/wt/feature", "/etc/hosts", "/etc/hosts"),
            (
                "/home/colin/wt/feature",
                "../../../etc/shadow.conf",
                "/home/etc/shadow.conf",
            ),
            ("/home", "../../../etc/passwd", "/etc/passwd"),
        ] {
            assert_eq!(
                resolve(Path::new(cwd), path),
                PathBuf::from(expected),
                "resolving {path:?} against {cwd:?}"
            );
        }
    }
}
