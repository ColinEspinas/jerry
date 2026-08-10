//! How a theme file is *written* - the layout, ordering and comments that make
//! `assets/themes/*.toml` and `~/.config/jerry/themes/*.toml` pleasant to open and hand-edit.
//!
//! Reading a theme file is `crate::settings::custom_theme`'s job and is deliberately liberal: key
//! order, grouping and comments carry no meaning at all, so a hand-edited file never has to look
//! like what this module produces. This module only decides what *Jerry* writes - when it
//! generates the five bundled themes, canonicalizes an import, converts a VSCode theme, or exports
//! one for sharing.
//!
//! ## Where the comments come from
//!
//! Every explanatory comment in a written file is parsed out of `crate::theme`'s own source at
//! startup ([`SOURCE`]), never hand-maintained here. A module's section blurb is that module's own
//! `///` doc comment; a key's trailing note is the token's own trailing `//` comment, or the first
//! sentence of its `///` docs. That matters because the alternative - a second table of
//! descriptions living in this module - would be a copy that silently drifts the first time
//! someone retunes a token and updates only the doc comment next to it. Here, the doc comment next
//! to the token *is* what the file says.
//!
//! ## Ordering
//!
//! [`SECTIONS`] lays the modules out in the order a person reading a theme top to bottom would
//! want them - the big surfaces first, then text, then what colour *means* in this app, then code,
//! then the smaller chrome - rather than in `crate::theme::TOKEN_GROUPS`' declaration order. A
//! module that isn't listed there still gets written (under a trailing "Other" section), so adding
//! one to `crate::theme` can never silently drop its keys out of generated files.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::settings::custom_theme::CustomThemeFile;

/// `crate::theme`'s own real source, embedded at compile time - the single source every comment in
/// a written theme file is derived from. See the module docs.
const SOURCE: &str = include_str!("../theme.rs");

/// The written layout: a section heading, and the `crate::theme` modules that belong under it.
///
/// Grouped by what a person is actually looking for when they open a theme file. Surfaces and
/// borders first because they are what a new theme changes first and what dominates the screen;
/// text next; then the tokens where colour carries *meaning* rather than structure (agent status,
/// diffs, tags) which are the ones you must not casually recolour; then the code surface; then the
/// long tail of small chrome.
const SECTIONS: &[(&str, &[&str])] = &[
    (
        "Surfaces and structure",
        &["surface", "border", "tree", "scrollbar"],
    ),
    ("Text", &["text"]),
    (
        "Meaning and state (colour carries information here)",
        &["status", "diff", "tag", "rail", "agent"],
    ),
    (
        "The code surface",
        &["syntax", "editor", "term", "terminal", "completions_popup"],
    ),
    (
        "Chrome and widgets",
        &[
            "button", "toggle", "settings", "palette", "graph", "env", "lang",
        ],
    ),
];

/// One module's parsed presentation data: its section blurb, and a note per key.
#[derive(Default)]
struct ModuleDocs {
    summary: Option<String>,
    /// Keyed by the token's own full key (`"surface.window"`).
    key_notes: HashMap<String, String>,
}

/// Everything parsed out of [`SOURCE`], once.
static DOCS: LazyLock<HashMap<String, ModuleDocs>> = LazyLock::new(parse_source_docs);

/// Turns a Rust doc fragment into something that reads well as a TOML comment: drops rustdoc link
/// brackets (`[`foo`]` -> `foo`), backticks, and collapses whitespace.
fn clean(fragment: &str) -> String {
    let mut out = String::with_capacity(fragment.len());
    let mut chars = fragment.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' => {}
            '[' => {
                // `[`super::border::DIVIDER`]` -> `border::DIVIDER`; a plain `[text]` keeps text.
                let mut inner = String::new();
                for next in chars.by_ref() {
                    if next == ']' {
                        break;
                    }
                    inner.push(next);
                }
                let inner = inner.replace('`', "");
                out.push_str(inner.trim_start_matches("super::"));
                // Skip a trailing rustdoc link target, `[foo](bar)`.
                if chars.peek() == Some(&'(') {
                    for next in chars.by_ref() {
                        if next == ')' {
                            break;
                        }
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first sentence of a doc block - what a one-line comment should say.
fn first_sentence(doc: &str) -> String {
    let cleaned = clean(doc);
    match cleaned.find(". ") {
        Some(end) => cleaned[..=end].trim_end().to_string(),
        None => cleaned,
    }
}

/// The longest a per-key note may be. Past this it stops helping and starts pushing the values
/// themselves off screen, so the key is written bare instead.
const MAX_KEY_NOTE: usize = 60;

/// Turns a token's own doc into a note worth putting next to a value in a theme file, or `None`.
///
/// A doc comment is written for someone editing `crate::theme`, not for someone editing a theme,
/// so some of them are whole paragraphs of design history and cite things (`Jerry.dc.html`, issue
/// numbers, other tokens) a theme author has no use for. This keeps the short, genuinely
/// descriptive ones - which is most of the palette, since the tokens ported from the design
/// handoff carry real one-line labels like `window body` - and drops the rest rather than
/// wrapping an essay into the file. No note is better than a wall of text.
fn key_note_from(doc: &str) -> Option<String> {
    let sentence = first_sentence(doc);
    // A leading "GitHub issue #141:" tells a theme author nothing.
    let sentence = match sentence.split_once(": ") {
        Some((prefix, rest)) if prefix.starts_with("GitHub issue #") => rest.to_string(),
        _ => sentence,
    };
    let sentence = sentence.trim().to_string();
    if sentence.is_empty() {
        return None;
    }
    if sentence.chars().count() <= MAX_KEY_NOTE {
        return Some(sentence);
    }
    // This module's docs almost always lead with a short label and then qualify it after a dash
    // ("The hint-size keycap's own background - distinct from ..."). When the whole sentence is
    // too long, that leading label is usually exactly the note a theme file wants.
    let head = sentence.split(" - ").next().unwrap_or_default().trim();
    let head = head.trim_end_matches(',').to_string();
    (!head.is_empty() && head.chars().count() <= MAX_KEY_NOTE).then_some(head)
}

/// Parses [`SOURCE`] into per-module summaries and per-key notes - see the module docs for why
/// this reads the real source rather than a table maintained here.
fn parse_source_docs() -> HashMap<String, ModuleDocs> {
    let mut docs: HashMap<String, ModuleDocs> = HashMap::new();
    let mut module = String::new();
    let mut pending_doc: Vec<String> = Vec::new();

    let mut pending_lines = SOURCE.lines().peekable();
    while let Some(line) = pending_lines.next() {
        let trimmed = line.trim_start();

        if let Some(doc) = trimmed
            .strip_prefix("/// ")
            .or_else(|| (trimmed == "///").then_some(""))
        {
            pending_doc.push(doc.to_string());
            continue;
        }

        if let Some(rest) = line.strip_prefix("pub mod ") {
            if let Some(name) = rest.strip_suffix(" {") {
                module = name.to_string();
                let summary = (!pending_doc.is_empty())
                    .then(|| first_sentence(&pending_doc.join(" ")))
                    .filter(|text| !text.is_empty());
                docs.entry(module.clone()).or_default().summary = summary;
            }
            pending_doc.clear();
            continue;
        }

        // A token declaration. Usually one line - `pub const NAME: ColorToken = token("k", 0xhex);
        // // note` - but rustfmt wraps the `(fg, bg)` pairs and the arrays across several, so the
        // whole statement is gathered before anything is read off it. Missing that is exactly how
        // an earlier version of this parser silently gave *no* note to any pair or array key.
        if trimmed.starts_with("pub const ") {
            let mut statement = line.to_string();
            while !statement.trim_end().ends_with(';') && !statement.contains("); //") {
                match pending_lines.next() {
                    Some(next) => {
                        statement.push(' ');
                        statement.push_str(next.trim());
                    }
                    None => break,
                }
            }
            if !statement.contains("token(\"") {
                pending_doc.clear();
                continue;
            }
            let line = statement.as_str();
            let note = trailing_comment(line)
                .map(|note| clean(&note))
                .filter(|note| {
                    !note.is_empty()
                        && note.chars().count() <= MAX_KEY_NOTE
                        && !is_uninformative(note)
                })
                .or_else(|| {
                    (!pending_doc.is_empty())
                        .then(|| key_note_from(&pending_doc.join(" ")))
                        .flatten()
                });
            if let Some(note) = note {
                let entry = docs.entry(module.clone()).or_default();
                for key in token_keys_on(line) {
                    entry.key_notes.entry(key).or_insert_with(|| note.clone());
                }
            }
            pending_doc.clear();
            continue;
        }

        if !trimmed.is_empty() {
            pending_doc.clear();
        }
    }
    docs
}

/// Every `token("...")` key declared on one source line - one for a plain token, two for a
/// `(fg, bg)` pair.
fn token_keys_on(line: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("token(\"") {
        rest = &rest[start + "token(\"".len()..];
        if let Some(end) = rest.find('"') {
            keys.push(rest[..end].to_string());
            rest = &rest[end..];
        } else {
            break;
        }
    }
    keys
}

/// Whether a note says nothing a reader couldn't already see from the key itself - `(fg, bg)` on
/// a pair token being the real example. Dropping these keeps the generated files free of comments
/// that restate their own key.
fn is_uninformative(note: &str) -> bool {
    let words: Vec<String> = note
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_ascii_lowercase())
        .collect();
    !words.is_empty() && words.iter().all(|word| word == "fg" || word == "bg")
}

/// A `// ...` comment trailing real code on `line` - deliberately not one *inside* the string
/// literal, which is why this looks past the last quote.
fn trailing_comment(line: &str) -> Option<String> {
    let after_code = line.rfind(");")? + 2;
    let tail = line.get(after_code..)?.trim();
    tail.strip_prefix("//").map(|note| note.trim().to_string())
}

/// The section blurb written above `[module]`, straight from that module's own doc comment.
pub fn module_summary(module: &str) -> Option<&'static str> {
    // `DOCS` is a `static`, so borrowing it yields a genuinely `'static` reference to the parsed
    // map - no `unsafe` and no leak needed to hand out `&'static str`s from it.
    let docs: &'static HashMap<String, ModuleDocs> = &DOCS;
    docs.get(module).and_then(|docs| docs.summary.as_deref())
}

/// The note written after a key's value, straight from that token's own comment or docs.
pub fn key_note(key: &str) -> Option<&'static str> {
    let (module, _) = key.split_once('.')?;
    let docs: &'static HashMap<String, ModuleDocs> = &DOCS;
    docs.get(module)
        .and_then(|docs| docs.key_notes.get(key))
        .map(String::as_str)
}

/// A TOML string literal, escaped by `toml` itself rather than by `format!`.
fn quoted(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

/// Wraps `text` into `# ` comment lines no wider than `width`, each prefixed with `indent`.
fn comment_block(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut line = String::from("#");
    for word in text.split_whitespace() {
        if line.len() + 1 + word.len() > width && line.len() > 1 {
            out.push_str(&line);
            out.push('\n');
            line = String::from("#");
        }
        line.push(' ');
        line.push_str(word);
    }
    if line.len() > 1 {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The written width theme files are laid out to.
const WIDTH: usize = 96;

/// Writes one theme file's real TOML text: a short header, the identity fields, then one
/// `[module]` table per group of keys - laid out in [`SECTIONS`]' reading order, with each
/// section's and each key's own explanation pulled from `crate::theme`'s source.
///
/// `header` is the file-specific preamble (what this particular theme is, and for a generated one,
/// how it was generated); the format explainer below it is the same for every file.
pub fn write_theme_toml(file: &CustomThemeFile, header: &str) -> String {
    let mut out = String::new();

    if !header.is_empty() {
        out.push_str(header);
        if !header.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("#\n");
    }
    out.push_str(&comment_block(
        "Every key below is optional. Anything this file does not name is inherited from the theme \
         in `base`, and ultimately from Jerry Dark's own built-in value - so deleting a line is a \
         real, supported edit, and a file that sets three keys is a complete theme. New keys added \
         by future Jerry versions simply inherit too; nothing here has to be kept exhaustive.",
        WIDTH,
    ));
    out.push('\n');

    out.push_str(&format!("name = {}\n", quoted(&file.name)));
    if !file.subtitle.is_empty() {
        out.push_str(&format!("subtitle = {}\n", quoted(&file.subtitle)));
    }
    if let Some(base) = &file.base {
        out.push_str(&format!("base = {}\n", quoted(base)));
    }
    if let Some(preview) = &file.preview {
        out.push_str("\n# The five swatches this theme's card shows on the Themes page.\n");
        let entries: Vec<String> = preview.iter().map(|value| quoted(value)).collect();
        out.push_str(&format!("preview = [{}]\n", entries.join(", ")));
    }

    // Group this file's entries by table, preserving each table's first-appearance order for any
    // module `SECTIONS` doesn't mention.
    let mut tables: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
    for (key, value) in &file.overrides {
        let (table, entry) = key.split_once('.').unwrap_or((key.as_str(), ""));
        match tables.iter_mut().find(|(name, _)| *name == table) {
            Some((_, entries)) => entries.push((entry, value.as_str())),
            None => tables.push((table, vec![(entry, value.as_str())])),
        }
    }

    let mut written: Vec<&str> = Vec::new();
    for (section, modules) in SECTIONS {
        let present: Vec<&(&str, Vec<(&str, &str)>)> = modules
            .iter()
            .filter_map(|module| tables.iter().find(|(name, _)| name == module))
            .collect();
        if present.is_empty() {
            continue;
        }
        out.push_str(&format!("\n\n# {}\n", "─".repeat(WIDTH - 2)));
        out.push_str(&format!("# {section}\n"));
        out.push_str(&format!("# {}\n", "─".repeat(WIDTH - 2)));
        for (module, entries) in present {
            written.push(module);
            write_table(&mut out, module, entries);
        }
    }
    // Anything `SECTIONS` doesn't place still gets written - a new `crate::theme` module must
    // never silently vanish from a generated file just because this layout wasn't updated.
    let leftovers: Vec<&(&str, Vec<(&str, &str)>)> = tables
        .iter()
        .filter(|(name, _)| !written.contains(name))
        .collect();
    if !leftovers.is_empty() {
        out.push_str(&format!(
            "\n\n# {}\n# Other\n# {}\n",
            "─".repeat(WIDTH - 2),
            "─".repeat(WIDTH - 2)
        ));
        for (module, entries) in leftovers {
            write_table(&mut out, module, entries);
        }
    }
    out
}

/// One `[module]` table: its blurb, then its keys with `=` aligned and each key's own note.
fn write_table(out: &mut String, module: &str, entries: &[(&str, &str)]) {
    out.push('\n');
    if let Some(summary) = module_summary(module) {
        out.push_str(&comment_block(summary, WIDTH));
    }
    out.push_str(&format!("[{module}]\n"));

    let quoted_keys: Vec<String> = entries
        .iter()
        .map(|(entry, _)| {
            // A pair/array key contains a dot (`sonnet.fg`, `lanes.0`) and must be quoted, or TOML
            // reads it as a nested table.
            if entry.contains('.') {
                quoted(entry)
            } else {
                (*entry).to_string()
            }
        })
        .collect();
    let widest = quoted_keys.iter().map(String::len).max().unwrap_or(0);

    for (index, (entry, value)) in entries.iter().enumerate() {
        let key = &quoted_keys[index];
        let note = key_note(&format!("{module}.{entry}"))
            .map(|note| format!("  # {note}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{key:<widest$} = {}{note}\n",
            quoted(value),
            widest = widest
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_colour_module_has_a_real_summary_parsed_from_the_source() {
        for (module, _) in crate::theme::TOKEN_GROUPS {
            assert!(
                module_summary(module).is_some(),
                "`{module}` has no doc comment in theme.rs, so a generated theme file would have \
                 an unexplained [{module}] section - add one there, not a description here"
            );
        }
    }

    #[test]
    fn key_notes_are_parsed_from_real_trailing_comments_and_doc_comments() {
        // A trailing `// window body` comment on the token line.
        assert_eq!(key_note("surface.window"), Some("window body"));
        // A `///` doc comment's first sentence.
        let keycap_hint = key_note("surface.keycap_hint").expect("this token is documented");
        assert!(
            keycap_hint.starts_with("The hint-size keycap's own background"),
            "got {keycap_hint:?}"
        );
        // Rustdoc link brackets and backticks are cleaned out.
        assert!(!keycap_hint.contains('['), "got {keycap_hint:?}");
        assert!(!keycap_hint.contains('`'), "got {keycap_hint:?}");
    }

    /// A `(fg, bg)` pair and an array are declared across several source lines by rustfmt, so the
    /// parser has to gather a whole statement rather than read one line - an earlier version did
    /// not, and silently gave every pair and array key no note at all.
    #[test]
    fn a_multi_line_pair_or_array_declaration_is_really_parsed() {
        // `lang::RS`'s own trailing `// "rs"` label, on a wrapped two-line declaration.
        assert_eq!(key_note("lang.rs.fg"), key_note("lang.rs.bg"));
        assert!(
            key_note("lang.rs.fg").is_some(),
            "a wrapped pair declaration must still be parsed"
        );
        // `graph::LANES`' own doc comment, shared by all six elements.
        assert!(key_note("graph.lanes.0").is_some());
        assert_eq!(key_note("graph.lanes.0"), key_note("graph.lanes.5"));
    }

    #[test]
    fn a_meaningful_share_of_all_keys_carry_a_real_note() {
        let documented = crate::theme::all_tokens()
            .filter(|token| key_note(token.key).is_some())
            .count();
        let total = crate::theme::all_tokens().count();
        // A real floor on the parser, not a documentation-coverage target: most tokens' doc
        // comments are design history written for someone editing `crate::theme`, and are
        // deliberately dropped rather than wrapped into a theme file (see `key_note_from`). The
        // section blurbs carry the explanatory weight; per-key notes are a bonus where a genuinely
        // short one exists.
        assert!(
            documented >= 50,
            "only {documented}/{total} keys carry a note - the source parser has probably stopped \
             matching declarations rather than the palette having lost its docs"
        );
    }

    #[test]
    fn a_note_that_only_restates_its_own_key_is_dropped() {
        assert!(is_uninformative("(fg, bg)"));
        assert!(is_uninformative("fg"));
        assert!(!is_uninformative("window body"));
        assert!(!is_uninformative("needs input"));
        assert_eq!(
            key_note("agent.sonnet.fg"),
            None,
            "`(fg, bg)` tells a theme author nothing the key doesn't already say"
        );
    }

    #[test]
    fn clean_strips_rustdoc_syntax_without_eating_the_words() {
        assert_eq!(clean("a `code` word"), "a code word");
        assert_eq!(
            clean("see [`super::border::DIVIDER`]"),
            "see border::DIVIDER"
        );
        assert_eq!(clean("see [the docs](https://example.com)"), "see the docs");
        assert_eq!(clean("  collapsed   whitespace "), "collapsed whitespace");
    }

    #[test]
    fn first_sentence_stops_at_the_first_full_stop() {
        assert_eq!(first_sentence("One thing. Then more."), "One thing.");
        assert_eq!(first_sentence("No full stop here"), "No full stop here");
    }

    /// Every module `crate::theme` declares is really placed by [`SECTIONS`] - a module missing
    /// from the layout still gets written (under "Other"), but that is a fallback, not the intent.
    #[test]
    fn every_registered_module_is_placed_in_a_real_section() {
        for (module, _) in crate::theme::TOKEN_GROUPS {
            assert!(
                SECTIONS.iter().any(|(_, modules)| modules.contains(module)),
                "`{module}` isn't placed in any SECTIONS entry - it would be written under \
                 \"Other\" rather than where a reader would look for it"
            );
        }
    }
}
