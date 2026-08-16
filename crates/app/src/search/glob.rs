//! The pure glob matcher behind the search panel's `include` / `exclude` path filters - the
//! design's "funnel for path filters", with include/exclude globs behind it (`src/**, tests/**`
//! / `target/**, *.lock`).
//!
//! ## Why hand-rolled rather than `globset`
//!
//! The two fields hold a *comma-separated list* of shell-style patterns matched against one
//! worktree-relative, `/`-separated path each. That is a genuinely small language (`**`, `*`,
//! `?`), and this crate already declines to add a dependency for something this size - see
//! `crate::sidebar::file_ops`'s own hand-rolled name validation, and
//! `crate::palette::state::substring_match`. `globset` would also bring `aho-corasick` and
//! `bstr` in for it. Being pure and dependency-free is what lets every rule below be a real unit
//! test rather than a claim about a third party's semantics.
//!
//! ## The rules, exactly
//!
//! - A pattern is split on `/` into segments and matched segment-by-segment against the path's
//!   own segments.
//! - `**` matches **zero or more whole segments**, so `target/**` matches `target/debug/x.rs` and
//!   `src/**/mod.rs` matches both `src/mod.rs` and `src/a/b/mod.rs`.
//! - `*` matches any run of characters **within one segment** (never across a `/`), `?` matches
//!   exactly one character within one segment.
//! - A pattern containing **no** `/` is matched against every path's *basename*, i.e. `*.lock` is
//!   read as `**/*.lock`. That is the rule VS Code's own search globs use, and it is what makes
//!   the design's own `*.lock` example mean what a reader expects it to. It is also why `src` and
//!   `src/` are genuinely different patterns: the trailing slash is what anchors it.
//! - Everything else is anchored at the worktree root: `src/**` does not match `crates/src/x.rs`.
//! - Matching is case-sensitive, because paths on this app's primary platform are.
//!
//! An empty list matches nothing ([`GlobList::is_empty`]); the *caller* is what decides that an
//! empty `include` means "no include filter at all" rather than "exclude everything" - see
//! [`crate::search::engine::PathFilter`].

/// One parsed pattern: its segments, plus whether it was basename-only (no `/` anywhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob {
    segments: Vec<String>,
}

impl Glob {
    /// Parses one pattern. Returns `None` for a pattern that is empty once trimmed, so a trailing
    /// comma or a double comma in a field is simply ignored rather than becoming a pattern that
    /// matches everything (or nothing) by accident.
    pub fn parse(pattern: &str) -> Option<Self> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return None;
        }
        // The basename rule, applied once here rather than at every match: a pattern with no `/`
        // becomes `**/<pattern>`, which is exactly "match the basename anywhere".
        let normalized = if pattern.contains('/') {
            pattern.to_string()
        } else {
            format!("**/{pattern}")
        };
        let segments: Vec<String> = normalized
            .split('/')
            // `src//foo` and a trailing `src/` both produce empty segments that mean nothing;
            // dropping them keeps `target/` and `target` equivalent, which is what a user typing
            // a directory name expects.
            .filter(|segment| !segment.is_empty())
            .map(|segment| segment.to_string())
            .collect();
        if segments.is_empty() {
            return None;
        }
        Some(Glob { segments })
    }

    /// Whether `path` - already worktree-relative and `/`-separated - matches this pattern.
    pub fn matches(&self, path: &str) -> bool {
        let path_segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match_segments(&self.segments, &path_segments)
    }
}

/// `**` matches zero or more whole segments; every other segment matches exactly one.
///
/// Recursion is bounded by the pattern's own segment count (each non-`**` step consumes one path
/// segment, each `**` step recurses on a strictly shorter pattern), and real patterns are a
/// handful of segments long - see this module's own docs for why this is not `globset`.
fn match_segments(pattern: &[String], path: &[&str]) -> bool {
    let Some((head, rest)) = pattern.split_first() else {
        return path.is_empty();
    };
    if head == "**" {
        // Zero segments consumed, then one, then two... The zero case is what makes `src/**`
        // match `src` itself and `**/x` match a root-level `x`.
        return (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]));
    }
    match path.split_first() {
        Some((first, tail)) => wildcard_matches(head, first) && match_segments(rest, tail),
        None => false,
    }
}

/// `*`/`?` matching **within one segment**. Iterative with one backtrack point, the standard
/// linear-in-practice formulation - a recursive one is exponential on patterns like `a*a*a*b`,
/// which a user can type into a filter field by accident.
fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    // Where to resume from if the current `*` turns out to have consumed too little.
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            // Let the last `*` swallow one more character and retry from just after it.
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    // Trailing `*`s in the pattern are allowed to match nothing at all.
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// One field's whole comma-separated list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobList {
    globs: Vec<Glob>,
}

impl GlobList {
    /// Parses `src/**, tests/**` into its two real patterns. Whitespace around each entry is
    /// trimmed and empty entries are dropped, so a field the user is halfway through typing
    /// (`src/**,`) behaves as the one pattern it currently states rather than briefly matching
    /// everything.
    pub fn parse(list: &str) -> Self {
        GlobList {
            globs: list.split(',').filter_map(Glob::parse).collect(),
        }
    }

    /// No real patterns at all - an empty field, or one holding only separators.
    pub fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }

    /// How many real patterns this list holds.
    pub fn len(&self) -> usize {
        self.globs.len()
    }

    /// Whether **any** pattern matches - a list is a union, which is what a comma reads as.
    pub fn matches(&self, path: &str) -> bool {
        self.globs.iter().any(|glob| glob.matches(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, path: &str) -> bool {
        Glob::parse(pattern)
            .unwrap_or_else(|| panic!("`{pattern}` must parse"))
            .matches(path)
    }

    #[test]
    fn a_double_star_matches_any_number_of_segments_including_none() {
        assert!(matches("target/**", "target/debug/build/x.rs"));
        assert!(matches("target/**", "target/x.rs"));
        assert!(
            matches("target/**", "target"),
            "zero segments is a real case: `target/**` naming the directory itself must not be a \
             surprise miss"
        );
        assert!(!matches("target/**", "src/target/x.rs"));
    }

    #[test]
    fn a_double_star_in_the_middle_spans_any_depth() {
        assert!(matches("src/**/mod.rs", "src/mod.rs"));
        assert!(matches("src/**/mod.rs", "src/a/mod.rs"));
        assert!(matches("src/**/mod.rs", "src/a/b/c/mod.rs"));
        assert!(!matches("src/**/mod.rs", "src/a/lib.rs"));
    }

    #[test]
    fn a_single_star_never_crosses_a_slash() {
        assert!(matches("src/*.rs", "src/lib.rs"));
        assert!(
            !matches("src/*.rs", "src/auth/session.rs"),
            "`*` is a within-segment wildcard - crossing a `/` is what `**` is for"
        );
    }

    #[test]
    fn a_pattern_with_no_slash_matches_the_basename_at_any_depth() {
        // The design's own `*.lock` example. Read literally (anchored at the root) it would match
        // only a root-level lock file, which is not what anyone typing it means.
        assert!(matches("*.lock", "Cargo.lock"));
        assert!(matches("*.lock", "crates/app/Cargo.lock"));
        assert!(!matches("*.lock", "crates/app/Cargo.toml"));
    }

    #[test]
    fn a_pattern_with_a_slash_is_anchored_at_the_worktree_root() {
        assert!(matches("src/**", "src/auth/session.rs"));
        assert!(
            !matches("src/**", "crates/src/auth/session.rs"),
            "an anchored pattern must not drift into a nested directory of the same name"
        );
    }

    #[test]
    fn a_question_mark_matches_exactly_one_character() {
        assert!(matches("src/?.rs", "src/a.rs"));
        assert!(!matches("src/?.rs", "src/ab.rs"));
        assert!(!matches("src/?.rs", "src/.rs"));
    }

    #[test]
    fn a_pathological_star_pattern_still_terminates_and_answers_correctly() {
        // The exponential case a naive recursive matcher blows up on - a user can type this.
        assert!(matches("a*a*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaab"));
        assert!(!matches("a*a*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaac"));
    }

    #[test]
    fn a_trailing_slash_is_what_anchors_a_directory_name_the_bare_name_does_not() {
        // These are deliberately *different*, and the difference is the basename rule doing its
        // job: `src/` contains a `/`, so it is a real, root-anchored path; a bare `src` is a
        // basename pattern that matches a `src` entry at any depth. Asserted rather than left
        // implicit because reading the two side by side, "the same thing" is the intuitive - and
        // wrong - answer.
        assert!(Glob::parse("src/").expect("parses").matches("src"));
        assert!(!Glob::parse("src/").expect("parses").matches("crates/src"));
        assert!(Glob::parse("src").expect("parses").matches("crates/src"));
    }

    #[test]
    fn a_repeated_or_trailing_separator_inside_a_pattern_is_not_a_segment() {
        assert!(Glob::parse("src//auth/**")
            .expect("parses")
            .matches("src/auth/session.rs"));
    }

    #[test]
    fn an_empty_or_separator_only_pattern_is_not_a_pattern() {
        assert_eq!(Glob::parse(""), None);
        assert_eq!(Glob::parse("   "), None);
        assert_eq!(Glob::parse("/"), None);
    }

    #[test]
    fn a_list_is_a_union_of_its_entries_and_ignores_stray_separators() {
        let list = GlobList::parse("src/**, tests/** ,");
        assert_eq!(list.len(), 2, "the trailing comma is not a third pattern");
        assert!(list.matches("src/auth/session.rs"));
        assert!(list.matches("tests/auth_race.rs"));
        assert!(!list.matches("migrations/0031.sql"));
    }

    #[test]
    fn an_empty_list_matches_nothing_and_says_so() {
        let list = GlobList::parse("   ");
        assert!(list.is_empty());
        assert!(
            !list.matches("anything.rs"),
            "an empty list is empty; turning it into \"match everything\" is the caller's \
             decision, not this type's - see `PathFilter`"
        );
    }

    #[test]
    fn matching_is_case_sensitive_like_the_paths_it_matches() {
        assert!(!matches("src/**", "SRC/lib.rs"));
    }
}
