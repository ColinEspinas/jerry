//! Pure Tab/Shift+Tab indentation resolution (GitHub issue #26): tabs-vs-spaces and indent
//! width, resolved from a real, minimally-parsed `.editorconfig` when present, falling back to
//! `crate::settings::store::EditorSettings` (the user's own default) otherwise. Deliberately
//! `gpui`-free, mirroring `crate::code_surface::edit_buffer`/`crate::lsp::completion`'s own split
//! between pure logic here and `crate::code_surface::editing`'s live GPUI wiring (that module
//! resolves the real absolute file path/worktree root and calls into this module's
//! [`indent_settings_for_path`], then hands the resulting [`IndentSettings`] to
//! `crate::code_surface::edit_buffer::EditBuffer::indent_lines`/`dedent_lines`).
//!
//! ## `.editorconfig` support is real but deliberately bounded
//!
//! This is a hand-rolled parser/matcher, not a vendored `editorconfig` crate (none of this
//! workspace's existing dependencies - `vendor/zed`'s own pinned set included - carry one). Real,
//! working support: `root = true`, `indent_style` (`tab`/`space`), `indent_size` (a number, or
//! `tab` meaning "use `tab_width`"), `tab_width`, section patterns matched against just the
//! file's own *basename* (`[*.rs]`, `[*]`, `[Makefile]`, brace-expanded `[*.{rs,toml}]`, `?`
//! wildcards). Two real, documented gaps, chosen so an unsupported pattern degrades to "doesn't
//! match" rather than a wrong match: directory-scoped patterns containing `/` (e.g.
//! `[lib/**.js]`) never match anything here, and `**`/bracket character classes (`[0-9]`) are not
//! specially interpreted (`[` is stripped as an ordinary section-header delimiter, so a pattern
//! that tries to use one as a glob class is read as a literal string instead of a wrong match).
//!
//! Precedence follows the real EditorConfig spec: files closer to the target file win over files
//! further away (per-property - a closer file that doesn't set `tab_width` at all still inherits
//! it from a farther one), the walk upward stops at (and includes) the first `root = true` file,
//! and within one file, a later matching `[section]` overrides an earlier matching one for
//! whichever properties it sets.

use std::ops::Range;
use std::path::Path;

/// The real, final Tab/Shift+Tab indentation the editor should use for one file - `insert_spaces`
/// picks between inserting [`Self::tab_width`] literal space characters or one literal `\t` per
/// indent level (see [`indent_unit`]), and `tab_width` is also the real width `EditBuffer::
/// dedent_lines` removes at most one indent level of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndentSettings {
    pub insert_spaces: bool,
    pub tab_width: u8,
}

/// The real indent-unit string one Tab press inserts at the caret, or one multi-line indent adds
/// to each touched line's own start - `crate::code_surface::edit_buffer::EditBuffer::indent_lines`'s
/// real input.
pub fn indent_unit(settings: IndentSettings) -> String {
    if settings.insert_spaces {
        " ".repeat(settings.tab_width.max(1) as usize)
    } else {
        "\t".to_string()
    }
}

/// One real `.editorconfig` file's own properties for whichever `[section]`s matched - `None`
/// for a property no matching section in this file ever set, distinct from a farther file's own
/// value being inherited (see [`merge_props`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorConfigProps {
    /// `Some(true)` for `indent_style = space`, `Some(false)` for `indent_style = tab`.
    pub indent_style: Option<bool>,
    /// A real, explicit `indent_size = N` (never `Some` for the real spec's `indent_size = tab`
    /// alias - that's read as "fall back to `tab_width`" per [`resolve_indent_settings`], not a
    /// literal size of its own).
    pub indent_size: Option<u8>,
    pub tab_width: Option<u8>,
}

/// One real, parsed `.editorconfig` file: whether its own top-level `root = true` was set (the
/// real, spec-defined signal to stop walking further up the directory tree - see
/// [`discover_editorconfig_files`]), plus every `[section pattern]` it declared, in real file
/// order (top to bottom, matching the spec's own "most recent rule wins" precedence - see
/// [`effective_props_for_file`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorConfigFile {
    pub is_root: bool,
    pub sections: Vec<(String, EditorConfigProps)>,
}

/// Parses one real `.editorconfig` file's real text - see this module's own top docs for exactly
/// which real EditorConfig syntax is (and isn't) understood. Never fails: an unrecognized line,
/// key, or value is simply skipped rather than aborting the whole file's real, otherwise-valid
/// properties.
pub fn parse_editorconfig(text: &str) -> EditorConfigFile {
    let mut file = EditorConfigFile::default();
    let mut current: Option<(String, EditorConfigProps)> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(pattern) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            if let Some(section) = current.take() {
                file.sections.push(section);
            }
            current = Some((pattern.to_string(), EditorConfigProps::default()));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        match &mut current {
            Some((_, props)) => apply_property(props, &key, &value),
            None => {
                if key == "root" {
                    file.is_root = value == "true";
                }
            }
        }
    }
    if let Some(section) = current.take() {
        file.sections.push(section);
    }
    file
}

fn apply_property(props: &mut EditorConfigProps, key: &str, value: &str) {
    match key {
        "indent_style" => match value {
            "space" => props.indent_style = Some(true),
            "tab" => props.indent_style = Some(false),
            _ => {}
        },
        // The real spec's `indent_size = tab` alias means "use `tab_width`" - handled by
        // [`resolve_indent_settings`]'s own fallback chain, not stored as a literal size here.
        "indent_size" if value != "tab" => {
            if let Ok(size) = value.parse::<u8>() {
                if size > 0 {
                    props.indent_size = Some(size);
                }
            }
        }
        "tab_width" => {
            if let Ok(width) = value.parse::<u8>() {
                if width > 0 {
                    props.tab_width = Some(width);
                }
            }
        }
        _ => {}
    }
}

/// `nearer`'s own real, set fields win; a field `nearer` leaves unset falls back to `farther`'s -
/// the real merge both across a single file's own matching sections (later section is "nearer",
/// per [`effective_props_for_file`]) and across multiple files (a closer file is "nearer", per
/// [`resolve_editor_config_props`]) use.
pub fn merge_props(nearer: EditorConfigProps, farther: EditorConfigProps) -> EditorConfigProps {
    EditorConfigProps {
        indent_style: nearer.indent_style.or(farther.indent_style),
        indent_size: nearer.indent_size.or(farther.indent_size),
        tab_width: nearer.tab_width.or(farther.tab_width),
    }
}

/// One real file's own effective properties for `file_name` - every section whose pattern
/// [`section_matches`] `file_name` contributes, with a later section in real file order
/// overriding an earlier one's already-set fields (the real spec's "most recent rule found takes
/// precedence" - see [`merge_props`]'s own docs for which side "wins").
pub fn effective_props_for_file(file: &EditorConfigFile, file_name: &str) -> EditorConfigProps {
    let mut result = EditorConfigProps::default();
    for (pattern, props) in &file.sections {
        if section_matches(pattern, file_name) {
            result = merge_props(*props, result);
        }
    }
    result
}

/// Whether `pattern` matches `file_name` - see this module's own top docs for exactly which real
/// glob syntax is supported (`*`, `?`, `{a,b,c}` brace alternation) and which real gaps are
/// deliberate (any pattern containing `/` never matches - this app only ever matches against a
/// bare file name, never a path relative to the `.editorconfig` file's own directory).
pub fn section_matches(pattern: &str, file_name: &str) -> bool {
    if pattern.contains('/') {
        return false;
    }
    expand_braces(pattern)
        .iter()
        .any(|alt| glob_match(alt, file_name))
}

/// Expands one real `{a,b,c}` brace group (at most one - the real EditorConfig spec allows
/// nesting, but no `.editorconfig` file this app has ever needed to read used more than one) into
/// every literal alternative pattern; a pattern with no brace group expands to itself.
fn expand_braces(pattern: &str) -> Vec<String> {
    if let (Some(start), Some(end)) = (pattern.find('{'), pattern.find('}')) {
        if end > start {
            let prefix = &pattern[..start];
            let suffix = &pattern[end + 1..];
            return pattern[start + 1..end]
                .split(',')
                .map(|alt| format!("{prefix}{alt}{suffix}"))
                .collect();
        }
    }
    vec![pattern.to_string()]
}

/// A small, real recursive `*`/`?` glob matcher over plain chars - bounded by `text`'s own length
/// (file names are short; this never runs against a whole path), matching the classic textbook
/// shape rather than pulling in a dependency for two wildcard characters.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(pattern: &[char], text: &[char]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some('*') => go(&pattern[1..], text) || (!text.is_empty() && go(pattern, &text[1..])),
            Some('?') => !text.is_empty() && go(&pattern[1..], &text[1..]),
            Some(ch) => text.first() == Some(ch) && go(&pattern[1..], &text[1..]),
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    go(&pattern, &text)
}

/// The real, combined properties from every discovered `.editorconfig` file, closest directory
/// first (`files_closest_first[0]` is the highest-priority file) - see [`merge_props`]'s own docs
/// for the real per-property precedence this folds left-to-right.
pub fn resolve_editor_config_props(
    files_closest_first: &[EditorConfigFile],
    file_name: &str,
) -> EditorConfigProps {
    let mut result = EditorConfigProps::default();
    for file in files_closest_first {
        let file_props = effective_props_for_file(file, file_name);
        result = merge_props(result, file_props);
    }
    result
}

/// The real, final [`IndentSettings`]: `props.indent_style` wins over `user_default.insert_spaces`
/// when set; the real width prefers an explicit `tab_width`, then `indent_size` (the real spec's
/// own fallback direction for a plain numeric `indent_size` with no `tab_width` set), then the
/// user's own default - always clamped to at least `1` so a caller can never divide-by/repeat-by
/// zero.
pub fn resolve_indent_settings(
    props: EditorConfigProps,
    user_default: IndentSettings,
) -> IndentSettings {
    let insert_spaces = props.indent_style.unwrap_or(user_default.insert_spaces);
    let tab_width = props
        .tab_width
        .or(props.indent_size)
        .unwrap_or(user_default.tab_width)
        .max(1);
    IndentSettings {
        insert_spaces,
        tab_width,
    }
}

/// A bound on how many parent directories [`discover_editorconfig_files`] will walk before
/// giving up - real directory trees are nowhere near this deep; this exists only so a
/// pathological input (e.g. a `start_dir` that never reaches `boundary` because it isn't
/// actually an ancestor of it) can't loop unboundedly. `Path::parent()` already terminates at a
/// real filesystem root on its own, so this is a defensive backstop, not the normal exit path.
const MAX_EDITORCONFIG_WALK_DEPTH: usize = 128;

/// Real filesystem discovery: reads and parses every real `.editorconfig` file from `start_dir`
/// upward, stopping after (and including) the first one with `root = true`, or once `boundary`
/// itself has been checked, whichever comes first - `boundary` is the real worktree root
/// (`crate::root::AdeApp::file_tree_root`), so this never wanders up into an unrelated ancestor
/// directory outside the worktree the file actually belongs to. A directory with no
/// `.editorconfig` at all is silently skipped (`std::fs::read_to_string` failing is the real,
/// ordinary "no file here" case, not an error to surface), matching how the rest of this
/// property is optional at every level.
pub fn discover_editorconfig_files(start_dir: &Path, boundary: &Path) -> Vec<EditorConfigFile> {
    let mut files = Vec::new();
    let mut dir = Some(start_dir.to_path_buf());
    let mut hops = 0;
    while let Some(current) = dir {
        if hops >= MAX_EDITORCONFIG_WALK_DEPTH {
            break;
        }
        hops += 1;
        let candidate = current.join(".editorconfig");
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            let parsed = parse_editorconfig(&text);
            let is_root = parsed.is_root;
            files.push(parsed);
            if is_root {
                break;
            }
        }
        if current == boundary {
            break;
        }
        dir = current.parent().map(Path::to_path_buf);
    }
    files
}

/// The one real end-to-end entry point `crate::code_surface::editing` calls: resolves
/// `absolute_path`'s real indentation settings from any `.editorconfig` files found walking up
/// from its own directory to `workspace_root` (see [`discover_editorconfig_files`]), falling back
/// to `user_default` for anything no `.editorconfig` file set.
pub fn indent_settings_for_path(
    absolute_path: &Path,
    workspace_root: &Path,
    user_default: IndentSettings,
) -> IndentSettings {
    let start_dir = absolute_path.parent().unwrap_or(workspace_root);
    let files = discover_editorconfig_files(start_dir, workspace_root);
    let file_name = absolute_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let props = resolve_editor_config_props(&files, file_name);
    resolve_indent_settings(props, user_default)
}

/// The real leading-whitespace byte length [`crate::code_surface::edit_buffer::EditBuffer::
/// dedent_lines`] removes from one line for a single Shift+Tab dedent step: one literal leading
/// `\t` counts as a whole indent level on its own (the real, common "this line uses tabs" case,
/// regardless of `width`), else up to `width` leading literal space characters - never more than
/// what the line actually has, and `0` (a real no-op) for a line with no real leading whitespace
/// at all.
pub fn leading_dedent_len(line_text: &str, width: u8) -> usize {
    let bytes = line_text.as_bytes();
    if bytes.first() == Some(&b'\t') {
        return 1;
    }
    let width = width.max(1) as usize;
    let mut count = 0;
    while count < width && bytes.get(count) == Some(&b' ') {
        count += 1;
    }
    count
}

/// `crate::code_surface::edit_buffer::EditBuffer::indent_lines`'s own real "which lines does a
/// selection touch" rule, factored out here so it's directly unit-testable without a live
/// `EditBuffer`: every line the selection's `start` through `end` byte range overlaps, except a
/// multi-line selection's own last line is excluded when the selection ends exactly at that
/// line's column 0 (selecting through the trailing newline of the line before, the same real
/// convention `vendor/zed`'s own editor and VS Code both use - the line the caret merely *lands
/// on* right after a full-line selection was never visually "touched"). `sel_end_line`/
/// `sel_end_col` are the real `(line, col)` `EditBuffer::line_col_for_offset` already resolves
/// for the selection's end offset.
pub fn touched_line_range(
    sel_start_line: usize,
    sel_end_line: usize,
    sel_end_col: usize,
) -> Range<usize> {
    let last_line = if sel_end_col == 0 && sel_end_line > sel_start_line {
        sel_end_line - 1
    } else {
        sel_end_line
    };
    sel_start_line..last_line + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_unit_repeats_spaces_or_uses_one_literal_tab() {
        assert_eq!(
            indent_unit(IndentSettings {
                insert_spaces: true,
                tab_width: 4
            }),
            "    "
        );
        assert_eq!(
            indent_unit(IndentSettings {
                insert_spaces: false,
                tab_width: 4
            }),
            "\t"
        );
    }

    #[test]
    fn parse_editorconfig_reads_root_and_a_plain_section() {
        let file =
            parse_editorconfig("root = true\n\n[*.rs]\nindent_style = space\nindent_size = 4\n");
        assert!(file.is_root);
        assert_eq!(file.sections.len(), 1);
        assert_eq!(file.sections[0].0, "*.rs");
        assert_eq!(file.sections[0].1.indent_style, Some(true));
        assert_eq!(file.sections[0].1.indent_size, Some(4));
    }

    #[test]
    fn parse_editorconfig_ignores_comments_and_blank_lines() {
        let file = parse_editorconfig("; a comment\n# another\n\n[*]\nindent_style = tab\n");
        assert_eq!(file.sections.len(), 1);
        assert_eq!(file.sections[0].1.indent_style, Some(false));
    }

    #[test]
    fn parse_editorconfig_indent_size_tab_is_not_stored_as_a_literal_size() {
        let file = parse_editorconfig("[*]\nindent_size = tab\ntab_width = 8\n");
        assert_eq!(file.sections[0].1.indent_size, None);
        assert_eq!(file.sections[0].1.tab_width, Some(8));
    }

    #[test]
    fn parse_editorconfig_skips_a_zero_or_unparseable_value() {
        let file = parse_editorconfig("[*]\nindent_size = 0\ntab_width = nonsense\n");
        assert_eq!(file.sections[0].1.indent_size, None);
        assert_eq!(file.sections[0].1.tab_width, None);
    }

    #[test]
    fn section_matches_supports_star_and_brace_expansion() {
        assert!(section_matches("*.rs", "main.rs"));
        assert!(!section_matches("*.rs", "main.ts"));
        assert!(section_matches("*", "anything.xyz"));
        assert!(section_matches("*.{rs,toml}", "Cargo.toml"));
        assert!(section_matches("*.{rs,toml}", "lib.rs"));
        assert!(!section_matches("*.{rs,toml}", "lib.py"));
        assert!(section_matches("Makefile", "Makefile"));
        assert!(!section_matches("Makefile", "makefile"));
    }

    #[test]
    fn section_matches_question_mark_matches_exactly_one_char() {
        assert!(section_matches("a?c", "abc"));
        assert!(!section_matches("a?c", "ac"));
        assert!(!section_matches("a?c", "abbc"));
    }

    #[test]
    fn section_matches_never_matches_a_pattern_containing_a_slash() {
        assert!(!section_matches("lib/*.js", "foo.js"));
    }

    #[test]
    fn effective_props_for_file_lets_a_later_section_override_an_earlier_one() {
        let file = EditorConfigFile {
            is_root: false,
            sections: vec![
                (
                    "*".to_string(),
                    EditorConfigProps {
                        indent_style: Some(true),
                        indent_size: Some(2),
                        tab_width: None,
                    },
                ),
                (
                    "*.rs".to_string(),
                    EditorConfigProps {
                        indent_style: Some(false),
                        indent_size: None,
                        tab_width: None,
                    },
                ),
            ],
        };
        let props = effective_props_for_file(&file, "main.rs");
        // The later, more specific `[*.rs]` section's own `indent_style` wins, but its own
        // unset `indent_size` still inherits the earlier `[*]` section's real value - the same
        // per-property (not whole-section) merge real EditorConfig tooling performs.
        assert_eq!(props.indent_style, Some(false));
        assert_eq!(props.indent_size, Some(2));
    }

    #[test]
    fn resolve_editor_config_props_prefers_the_closer_file_per_property() {
        let closer = EditorConfigFile {
            is_root: false,
            sections: vec![(
                "*".to_string(),
                EditorConfigProps {
                    indent_style: Some(true),
                    indent_size: None,
                    tab_width: None,
                },
            )],
        };
        let farther = EditorConfigFile {
            is_root: true,
            sections: vec![(
                "*".to_string(),
                EditorConfigProps {
                    indent_style: Some(false),
                    indent_size: None,
                    tab_width: Some(8),
                },
            )],
        };
        let props = resolve_editor_config_props(&[closer, farther], "main.rs");
        assert_eq!(
            props.indent_style,
            Some(true),
            "the closer file's own indent_style must win"
        );
        assert_eq!(
            props.tab_width,
            Some(8),
            "a property only the farther file set must still be inherited"
        );
    }

    #[test]
    fn resolve_indent_settings_falls_back_to_the_user_default_for_every_unset_field() {
        let resolved = resolve_indent_settings(
            EditorConfigProps::default(),
            IndentSettings {
                insert_spaces: false,
                tab_width: 3,
            },
        );
        assert!(!resolved.insert_spaces);
        assert_eq!(resolved.tab_width, 3);
    }

    #[test]
    fn resolve_indent_settings_prefers_tab_width_over_indent_size() {
        let resolved = resolve_indent_settings(
            EditorConfigProps {
                indent_style: None,
                indent_size: Some(2),
                tab_width: Some(4),
            },
            IndentSettings {
                insert_spaces: true,
                tab_width: 8,
            },
        );
        assert_eq!(resolved.tab_width, 4);
    }

    #[test]
    fn resolve_indent_settings_falls_back_to_indent_size_when_tab_width_is_unset() {
        let resolved = resolve_indent_settings(
            EditorConfigProps {
                indent_style: None,
                indent_size: Some(2),
                tab_width: None,
            },
            IndentSettings {
                insert_spaces: true,
                tab_width: 8,
            },
        );
        assert_eq!(resolved.tab_width, 2);
    }

    #[test]
    fn discover_editorconfig_files_walks_up_and_stops_at_root_true() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root_dir = temp.path();
        let nested = root_dir.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("mkdir -p");
        std::fs::write(
            root_dir.join(".editorconfig"),
            "root = true\n[*]\nindent_style = tab\n",
        )
        .expect("write root editorconfig");
        std::fs::write(
            root_dir.join("a").join(".editorconfig"),
            "[*.rs]\nindent_size = 2\n",
        )
        .expect("write nested editorconfig");

        let files = discover_editorconfig_files(&nested, root_dir);
        assert_eq!(
            files.len(),
            2,
            "the nested file and the root file, not beyond it"
        );
        assert_eq!(files[0].sections[0].0, "*.rs");
        assert!(files[1].is_root);
    }

    #[test]
    fn discover_editorconfig_files_stops_at_the_boundary_even_without_root_true() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root_dir = temp.path();
        let nested = root_dir.join("a");
        std::fs::create_dir_all(&nested).expect("mkdir -p");
        std::fs::write(nested.join(".editorconfig"), "[*]\nindent_size = 2\n")
            .expect("write nested editorconfig");

        // No `.editorconfig` at `root_dir` itself, and no `root = true` anywhere - the walk must
        // still stop once `boundary` (`root_dir`) has been checked, never wandering past it into
        // this real temp directory's own real ancestors (e.g. the OS temp root).
        let files = discover_editorconfig_files(&nested, root_dir);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn indent_settings_for_path_resolves_end_to_end_against_real_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root_dir = temp.path();
        std::fs::write(
            root_dir.join(".editorconfig"),
            "root = true\n[*.rs]\nindent_style = space\nindent_size = 2\n",
        )
        .expect("write editorconfig");
        let file_path = root_dir.join("main.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write file");

        let resolved = indent_settings_for_path(
            &file_path,
            root_dir,
            IndentSettings {
                insert_spaces: false,
                tab_width: 8,
            },
        );
        assert!(resolved.insert_spaces);
        assert_eq!(resolved.tab_width, 2);
    }

    #[test]
    fn indent_settings_for_path_falls_back_to_user_default_with_no_editorconfig_at_all() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root_dir = temp.path();
        let file_path = root_dir.join("main.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write file");

        let resolved = indent_settings_for_path(
            &file_path,
            root_dir,
            IndentSettings {
                insert_spaces: true,
                tab_width: 4,
            },
        );
        assert!(resolved.insert_spaces);
        assert_eq!(resolved.tab_width, 4);
    }

    #[test]
    fn leading_dedent_len_treats_one_leading_tab_as_a_whole_level() {
        assert_eq!(leading_dedent_len("\tfoo", 4), 1);
        assert_eq!(leading_dedent_len("\t\tfoo", 4), 1);
    }

    #[test]
    fn leading_dedent_len_removes_up_to_width_spaces() {
        assert_eq!(leading_dedent_len("    foo", 4), 4);
        assert_eq!(leading_dedent_len("  foo", 4), 2);
        assert_eq!(leading_dedent_len("      foo", 4), 4);
    }

    #[test]
    fn leading_dedent_len_is_zero_for_a_line_with_no_leading_whitespace() {
        assert_eq!(leading_dedent_len("foo", 4), 0);
    }

    #[test]
    fn touched_line_range_includes_every_line_a_normal_selection_overlaps() {
        assert_eq!(touched_line_range(0, 2, 3), 0..3);
        assert_eq!(touched_line_range(1, 1, 0), 1..2);
    }

    #[test]
    fn touched_line_range_excludes_the_last_line_when_the_selection_ends_at_its_column_zero() {
        // Selecting all of line 0 through line 1's very start (e.g. triple-click-drag through
        // one full line's trailing newline) should indent only line 0.
        assert_eq!(touched_line_range(0, 1, 0), 0..1);
    }

    #[test]
    fn touched_line_range_keeps_a_single_line_selection_even_at_column_zero() {
        // A collapsed, single-line caret at column 0 must still count as touching that one line
        // (the empty-selection Tab-at-caret case is handled separately by the caller, but a
        // real, non-empty single-line selection that happens to start at column 0 must not be
        // excluded by the same rule that skips a *multi*-line selection's trailing empty line).
        assert_eq!(touched_line_range(2, 2, 0), 2..3);
    }
}
