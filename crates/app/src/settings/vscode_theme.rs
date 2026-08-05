//! GitHub issue #141: importing a real VSCode theme JSON file and converting it into one of this
//! app's own theme files (`crate::settings::custom_theme`) - a real, per-token palette, not a
//! handful of swatches.
//!
//! ## What a converted theme actually contains
//!
//! Two layers, in this order, both written literally into the resulting `.toml`:
//!
//! 1. **A full derived base.** Five representative colours are picked out of the VSCode theme
//!    (`editor.background`, a sidebar/panel background, and three accents - see
//!    [`swatches_from_vscode`]) and run through `crate::theme::derive_shift`/`derived_palette`,
//!    producing a real value for every one of this app's ~270 tokens. This is what stops an
//!    imported theme from being a patchwork: a light VSCode theme re-tints *all* of Jerry's
//!    chrome, including the many tokens (border levels, the twelve `text::*` steps, rail and graph
//!    chrome, ...) no VSCode colour key has any equivalent for.
//! 2. **Every real, directly-mapped key on top.** [`COLOR_KEY_MAP`] maps this app's tokens onto
//!    the VSCode `colors` keys that genuinely mean the same thing, and [`build_syntax_overrides`]
//!    maps every syntax bucket onto the theme's own `tokenColors` textmate scopes. Wherever the
//!    theme really says something, its own literal colour wins over the derived one.
//!
//! Before the theme system's rewrite only the first layer existed for chrome (there was no
//! per-token override mechanism at all) and the second existed only for syntax. Both layers now
//! land in the same flat, hand-editable file, so an imported theme is exactly as adjustable
//! afterwards as a hand-written one.
//!
//! ## Which VSCode keys are mapped, and which deliberately aren't
//!
//! [`COLOR_KEY_MAP`] covers the parts of VSCode's colour surface that have a real counterpart
//! here: the editor and its gutter/selection/line highlight, the sidebar/activity bar/panel/status
//! bar/title bar backgrounds Jerry's own rail, panels, header and footer correspond to, list
//! rows (hover/active/inactive selection), input and dropdown surfaces, buttons, badges, the
//! sixteen-colour terminal ANSI palette, diff/git decoration colours, editor error/warning
//! squigglies, scrollbar slider states, and the four `foreground` text levels
//! (`foreground`/`descriptionForeground`/`disabledForeground`).
//!
//! Deliberately **not** mapped, and why: VSCode's peek view, notebook, testing, merge-conflict,
//! debug-toolbar, chart, and extension-button colour families have no counterpart in this app at
//! all (there is no such surface to paint); its `*.border` keys are mostly per-widget and would
//! flatten onto Jerry's four structural border levels in a way that reads worse than the derived
//! values; and its `editorBracketHighlight.foreground1..6` family maps onto nothing here because
//! this app has no bracket-pair colouring yet (`theme::editor::MATCHING_BRACKET` is a real token
//! but nothing paints it - see its own docs). Every one of those keeps its derived value, which is
//! a real colour in the theme's own family, not a Jerry Dark leftover.
//!
//! ## JSONC tolerance
//!
//! Real, downloaded VSCode theme files are JSONC (`//` line comments, `/* */` block comments, and
//! trailing commas - none of which plain JSON allows), not strict JSON - [`strip_jsonc_noise`]
//! is a small, real, string-aware stripper run before `serde_json::from_str`, not a guess that
//! `serde_json` alone would happen to tolerate real-world files.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::code_surface::code_view::HighlightKind;
use crate::theme;

use super::custom_theme::CustomThemeFile;

/// Jerry Dark's own three accent swatches - the last-resort default for an accent this VSCode
/// theme's own `colors`/`tokenColors` never name at all. Never used for `background`/`panel` - see
/// [`VscodeThemeError::MissingBackground`]'s own docs for why those two are load-bearing enough to
/// be real errors instead.
const DEFAULT_ACCENT_GREEN: &str = "#5cb87f";
const DEFAULT_ACCENT_AMBER: &str = "#e2a336";
const DEFAULT_ACCENT_BLUE: &str = "#74ade8";

/// Every real, specific way a VSCode theme JSON file can fail to convert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VscodeThemeError {
    Parse(String),
    /// Neither `colors["editor.background"]` nor a top-level `colors["background"]` parsed as a
    /// real colour - unlike the three accents (which fall back to a Jerry Dark default), a theme
    /// this app cannot even determine a background for is not a real conversion, just a guess
    /// wearing one.
    MissingBackground,
    /// An `include` chain longer than [`MAX_INCLUDE_DEPTH`] - a malformed or circular one.
    IncludeTooDeep {
        limit: usize,
    },
}

impl std::fmt::Display for VscodeThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VscodeThemeError::Parse(msg) => write!(f, "not a valid VSCode theme file: {msg}"),
            VscodeThemeError::MissingBackground => write!(
                f,
                "couldn't find a real `editor.background` (or top-level `background`) colour in \
                 this VSCode theme - nothing to convert"
            ),
            VscodeThemeError::IncludeTooDeep { limit } => write!(
                f,
                "this VSCode theme's `include` chain is more than {limit} files deep - it is \
                 probably circular"
            ),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct VscodeThemeFile {
    #[serde(default)]
    name: Option<String>,
    /// A real VSCode theme file may be defined as a delta on another one, by relative path
    /// (`"include": "./dark_vs.json"`). See [`load_vscode_theme_with_includes`] - this is not an
    /// exotic corner: it is how VSCode's *own* default themes are built.
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    colors: HashMap<String, String>,
    #[serde(default, rename = "tokenColors")]
    token_colors: Vec<VscodeTokenColorRule>,
}

impl VscodeThemeFile {
    /// Layers `self` on top of `base`, with VSCode's own semantics: `colors` merge key by key with
    /// the including file winning, and `tokenColors` are concatenated with the including file's
    /// rules *after* the base's, since a later rule of equal specificity wins (see
    /// [`first_scope_foreground`]).
    fn layered_over(mut self, base: VscodeThemeFile) -> VscodeThemeFile {
        let mut colors = base.colors;
        colors.extend(self.colors.drain());
        let mut token_colors = base.token_colors;
        token_colors.append(&mut self.token_colors);
        VscodeThemeFile {
            // The including file names the theme; only if it doesn't does the base's name apply.
            name: self.name.or(base.name),
            include: None,
            colors,
            token_colors,
        }
    }
}

/// How many `include` hops [`load_vscode_theme_with_includes`] will follow. VSCode's own default
/// themes use two (`dark_modern.json` -> `dark_plus.json` -> `dark_vs.json`); this is generous
/// headroom over anything real, and a hard stop rather than an unbounded walk over
/// attacker-supplied relative paths.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Reads a real VSCode theme file, following its `include` chain relative to its own directory.
///
/// This exists because it is genuinely load-bearing, not for completeness: VSCode's current
/// default dark theme, `dark_modern.json`, contains almost no `colors` of its own, and `Dark+`
/// (`dark_plus.json`) contains **none** - it is `tokenColors` plus `"include": "./dark_vs.json"`.
/// Without following that, importing the actual shipped Dark+ file failed outright with
/// [`VscodeThemeError::MissingBackground`], because as far as the converter could see the theme
/// defined no background at all. A real, user-reported bug.
///
/// A missing include target is a real error rather than a silent partial import: a theme that
/// resolves to "no colours" would otherwise convert into something that is not the theme the user
/// asked for. Cycles and runaway chains are bounded by [`MAX_INCLUDE_DEPTH`].
fn load_vscode_theme_with_includes(
    source_path: &Path,
    depth: usize,
) -> Result<VscodeThemeFile, VscodeThemeError> {
    let contents = std::fs::read_to_string(source_path)
        .map_err(|err| VscodeThemeError::Parse(format!("{}: {err}", source_path.display())))?;
    let file = parse_vscode_theme_str(&contents)?;
    let Some(include) = file.include.clone() else {
        return Ok(file);
    };
    if depth >= MAX_INCLUDE_DEPTH {
        return Err(VscodeThemeError::IncludeTooDeep {
            limit: MAX_INCLUDE_DEPTH,
        });
    }
    // Relative to the including file's own directory, exactly as VSCode resolves it.
    let base_path = source_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(include.trim_start_matches("./"));
    let base = load_vscode_theme_with_includes(&base_path, depth + 1)?;
    Ok(file.layered_over(base))
}

/// The shared JSONC-tolerant parse step - see [`strip_jsonc_noise`].
fn parse_vscode_theme_str(contents: &str) -> Result<VscodeThemeFile, VscodeThemeError> {
    let stripped = strip_jsonc_noise(contents);
    serde_json::from_str(&stripped).map_err(|err| VscodeThemeError::Parse(err.to_string()))
}

#[derive(Debug, Default, Deserialize)]
struct VscodeTokenColorRule {
    #[serde(default)]
    scope: Option<ScopeField>,
    #[serde(default)]
    settings: VscodeTokenSettings,
}

#[derive(Debug, Default, Deserialize)]
struct VscodeTokenSettings {
    #[serde(default)]
    foreground: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScopeField {
    One(String),
    Many(Vec<String>),
}

impl ScopeField {
    /// Every individual selector this rule declares - a VSCode `scope` is either one string
    /// (possibly comma-separated) or an array of them.
    fn selectors(&self) -> Vec<&str> {
        match self {
            ScopeField::One(scope) => scope.split(',').map(|part| part.trim()).collect(),
            ScopeField::Many(scopes) => scopes.iter().map(|scope| scope.trim()).collect(),
        }
    }

    /// How specifically this rule applies to the textmate scope `query`, or `None` if it doesn't
    /// apply at all - the length of the longest selector that is a real scope-prefix of `query`.
    ///
    /// The *direction* here is the whole correctness of this module's syntax mapping, and it is
    /// the opposite of what an earlier version did. A theme rule applies to a token when the
    /// rule's selector is a prefix of the token's scope, not the other way round: asking "what
    /// colour is a plain `variable`?" must **not** be answered by a rule for
    /// `variable.parameter` (that rule is about something more specific), while asking "what
    /// colour is `variable.parameter`?" *is* legitimately answered by a rule for `variable` when
    /// the theme has nothing more specific. Matching the loose way round silently gave
    /// `syntax.variable` a parameter's colour for any theme that styled parameters - a real bug
    /// this module's own `a_child_scope_the_theme_styles_diverges_from_the_parent_it_used_to_alias`
    /// test now pins.
    ///
    /// Prefixes are compared at `.`-segment boundaries, so `variable` matches
    /// `variable.parameter` but `var` does not match `variable`.
    fn specificity_for(&self, query: &str) -> Option<usize> {
        self.selectors()
            .into_iter()
            .filter(|selector| {
                !selector.is_empty()
                    && (query == *selector || query.starts_with(&format!("{selector}.")))
            })
            .map(|selector| selector.len())
            .max()
    }
}

/// The real mapping from this app's own token keys to the VSCode `colors` keys that genuinely
/// mean the same thing, most-specific first - the first key a given theme actually defines wins,
/// and a token whose whole list is absent simply keeps its derived value (see the module docs).
///
/// Every VSCode key here is a real one from VSCode's own published theme-colour reference, not a
/// guess. The mapping choices worth stating explicitly:
///
/// - Jerry's three background levels (`surface.window`/`surface.center`/`surface.pty`) all come
///   from `editor.background`, with the terminal surface preferring `terminal.background` and
///   `panel.background` when a theme defines them - those are genuinely the same surfaces.
/// - Jerry's rail, panel headers and footers correspond to VSCode's sidebar/activity bar/status
///   bar chrome, so they read from `sideBar.background` first and fall back through
///   `activityBar.background`/`panel.background`/`editorGroupHeader.tabsBackground`.
/// - `surface.row_hover`/`row_selected` map to VSCode's own list row states
///   (`list.hoverBackground`, `list.activeSelectionBackground` with the inactive variant as a
///   fallback), which is exactly what Jerry's file-tree and change rows are.
/// - The `status::*` family is agent urgency, not VSCode's status *bar*: green/amber/red/blue come
///   from the terminal ANSI palette (`terminal.ansiGreen`/`ansiYellow`/`ansiRed`/`ansiBlue`), the
///   most reliably-defined semantic colour set in a VSCode theme, with the editor's own
///   error/warning foregrounds as fallbacks.
/// - `term.*`'s sixteen-colour block maps one-to-one onto `terminal.ansi*`, the one place this
///   app and VSCode agree exactly.
/// - `diff.*` maps onto VSCode's own diff editor and git decoration colours; the `*_bg` tokens
///   prefer `diffEditor.insertedTextBackground`/`removedTextBackground` (which are often
///   translucent in VSCode - the alpha channel is dropped, see [`normalize_vscode_hex`], leaving
///   the intended hue) and fall back to the git decoration foregrounds.
const COLOR_KEY_MAP: &[(&str, &[&str])] = &[
    // ---- surfaces -------------------------------------------------------------------------
    ("surface.window", &["editor.background", "background"]),
    ("surface.center", &["editor.background"]),
    (
        "surface.card",
        &["editorWidget.background", "menu.background"],
    ),
    ("surface.card_sunk", &["input.background"]),
    (
        "surface.popover",
        &["editorWidget.background", "editorSuggestWidget.background"],
    ),
    (
        "surface.palette",
        &["quickInput.background", "editorWidget.background"],
    ),
    (
        "surface.pty",
        &[
            "terminal.background",
            "panel.background",
            "editor.background",
        ],
    ),
    (
        "surface.rail",
        &["sideBar.background", "activityBar.background"],
    ),
    (
        "surface.header",
        &["editorGroupHeader.tabsBackground", "tab.inactiveBackground"],
    ),
    ("surface.title_bar", &["titleBar.activeBackground"]),
    ("surface.footer", &["statusBar.background"]),
    ("surface.row_hover", &["list.hoverBackground"]),
    (
        "surface.row_hover_alt",
        &["toolbar.hoverBackground", "list.hoverBackground"],
    ),
    (
        "surface.row_selected",
        &[
            "list.activeSelectionBackground",
            "list.inactiveSelectionBackground",
        ],
    ),
    (
        "surface.menu_row_hover",
        &["menu.selectionBackground", "list.hoverBackground"],
    ),
    ("surface.current_line", &["editor.lineHighlightBackground"]),
    ("surface.chip_neutral", &["badge.background"]),
    ("surface.segment_track", &["input.background"]),
    (
        "surface.segment_active",
        &["inputOption.activeBackground", "button.secondaryBackground"],
    ),
    // `surface.scrim` is deliberately absent: it is a near-black sheet painted at 62% alpha behind
    // the command palette, and the closest VSCode key (`editor.background`) would make it the same
    // colour as the window it is supposed to be dimming - a mapping that reads as "the scrim
    // stopped working". Its derived value keeps the theme's own hue while staying a real dimmer.
    // ---- borders --------------------------------------------------------------------------
    ("border.zone", &["editorGroup.border", "panel.border"]),
    ("border.inner", &["panel.border", "editorGroup.border"]),
    ("border.divider", &["editorGroup.border", "panel.border"]),
    ("border.card", &["editorWidget.border", "menu.border"]),
    (
        "border.card_field",
        &["input.border", "editorWidget.border"],
    ),
    ("border.composer", &["input.border"]),
    (
        "border.popover",
        &["editorSuggestWidget.border", "editorWidget.border"],
    ),
    ("border.button", &["button.border", "contrastBorder"]),
    ("border.selected_edge", &["focusBorder"]),
    // ---- text -----------------------------------------------------------------------------
    (
        "text.selected",
        &["list.activeSelectionForeground", "editor.foreground"],
    ),
    ("text.primary", &["editor.foreground", "foreground"]),
    ("text.body", &["foreground", "editor.foreground"]),
    ("text.secondary", &["sideBar.foreground", "foreground"]),
    ("text.muted", &["descriptionForeground"]),
    ("text.dim", &["descriptionForeground"]),
    (
        "text.faint",
        &["disabledForeground", "descriptionForeground"],
    ),
    ("text.disabled", &["disabledForeground"]),
    ("text.gutter", &["editorLineNumber.foreground"]),
    (
        "text.dimmer",
        &["editorLineNumber.activeForeground", "descriptionForeground"],
    ),
    // ---- status (agent urgency - see this constant's own docs) -----------------------------
    (
        "status.review",
        &[
            "terminal.ansiGreen",
            "gitDecoration.addedResourceForeground",
        ],
    ),
    (
        "status.ask",
        &["terminal.ansiYellow", "editorWarning.foreground"],
    ),
    (
        "status.fail",
        &["terminal.ansiRed", "editorError.foreground"],
    ),
    ("status.run", &["terminal.ansiBlue", "textLink.foreground"]),
    (
        "status.idle",
        &["disabledForeground", "descriptionForeground"],
    ),
    // ---- editor chrome --------------------------------------------------------------------
    ("editor.selection", &["editor.selectionBackground"]),
    (
        "editor.selection_inactive",
        &["editor.inactiveSelectionBackground"],
    ),
    ("editor.current_line", &["editor.lineHighlightBackground"]),
    ("editor.caret", &["editorCursor.foreground"]),
    ("editor.gutter_text", &["editorLineNumber.foreground"]),
    (
        "editor.gutter_text_active",
        &["editorLineNumber.activeForeground"],
    ),
    (
        "editor.indent_guide",
        &[
            "editorIndentGuide.background1",
            "editorIndentGuide.background",
        ],
    ),
    (
        "editor.indent_guide_active",
        &[
            "editorIndentGuide.activeBackground1",
            "editorIndentGuide.activeBackground",
        ],
    ),
    (
        "editor.matching_bracket",
        &["editorBracketMatch.background"],
    ),
    ("editor.whitespace", &["editorWhitespace.foreground"]),
    ("editor.diff_added", &["editorGutter.addedBackground"]),
    ("editor.diff_removed", &["editorGutter.deletedBackground"]),
    ("editor.blame_text", &["descriptionForeground"]),
    // ---- syntax chrome that lives outside the tokenColors world -----------------------------
    ("syntax.caret", &["editorCursor.foreground"]),
    ("syntax.error_underline", &["editorError.foreground"]),
    (
        "syntax.hover_underline",
        &["textLink.foreground", "editorLink.activeForeground"],
    ),
    (
        "syntax.diagnostic_card_message",
        &["editorError.foreground"],
    ),
    // ---- terminal: the one exact one-to-one block -------------------------------------------
    ("term.text", &["terminal.foreground", "editor.foreground"]),
    ("term.dim", &["terminal.ansiBrightBlack"]),
    ("term.ok", &["terminal.ansiGreen"]),
    ("term.err", &["terminal.ansiRed"]),
    ("term.warn", &["terminal.ansiYellow"]),
    (
        "term.prompt",
        &["terminal.ansiBrightBlue", "terminal.ansiBlue"],
    ),
    ("term.activity", &["terminal.ansiBlue"]),
    (
        "term.heading",
        &["terminal.ansiBrightWhite", "terminal.foreground"],
    ),
    (
        "term.cursor",
        &["terminalCursor.foreground", "editorCursor.foreground"],
    ),
    ("term.link", &["textLink.foreground"]),
    (
        "term.link_hover",
        &["textLink.activeForeground", "textLink.foreground"],
    ),
    (
        "term.menu_sel_fg",
        &["terminal.ansiBrightYellow", "terminal.ansiYellow"],
    ),
    // ---- diff -------------------------------------------------------------------------------
    (
        "diff.add_bg",
        &[
            "diffEditor.insertedTextBackground",
            "diffEditor.insertedLineBackground",
        ],
    ),
    (
        "diff.del_bg",
        &[
            "diffEditor.removedTextBackground",
            "diffEditor.removedLineBackground",
        ],
    ),
    (
        "diff.add_fg",
        &[
            "gitDecoration.addedResourceForeground",
            "terminal.ansiGreen",
        ],
    ),
    (
        "diff.del_fg",
        &[
            "gitDecoration.deletedResourceForeground",
            "terminal.ansiRed",
        ],
    ),
    (
        "diff.add_sign",
        &["editorGutter.addedBackground", "terminal.ansiGreen"],
    ),
    (
        "diff.del_sign",
        &["editorGutter.deletedBackground", "terminal.ansiRed"],
    ),
    (
        "diff.stat_add",
        &[
            "gitDecoration.addedResourceForeground",
            "terminal.ansiGreen",
        ],
    ),
    (
        "diff.stat_del",
        &[
            "gitDecoration.deletedResourceForeground",
            "terminal.ansiRed",
        ],
    ),
    ("diff.git_gutter", &["editorGutter.modifiedBackground"]),
    ("diff.ctx_fg", &["descriptionForeground"]),
    // ---- buttons, toggles, badges -----------------------------------------------------------
    ("button.blue_bg", &["button.background"]),
    (
        "button.blue_bg_hover",
        &["button.hoverBackground", "button.background"],
    ),
    ("button.blue_fg", &["button.foreground"]),
    ("button.green_bg", &["terminal.ansiGreen"]),
    ("button.green_fg", &["button.foreground"]),
    ("button.amber_bg", &["terminal.ansiYellow"]),
    (
        "button.danger_fg",
        &["errorForeground", "editorError.foreground"],
    ),
    (
        "button.danger_fg_hover",
        &["editorError.foreground", "errorForeground"],
    ),
    ("toggle.track_on", &["button.background", "focusBorder"]),
    (
        "toggle.track_off",
        &["input.background", "badge.background"],
    ),
    // ---- scrollbar --------------------------------------------------------------------------
    ("scrollbar.thumb", &["scrollbarSlider.background"]),
    (
        "scrollbar.thumb_hover",
        &["scrollbarSlider.hoverBackground"],
    ),
    // ---- completions popup ------------------------------------------------------------------
    (
        "completions_popup.item_selected_bg",
        &[
            "editorSuggestWidget.selectedBackground",
            "list.activeSelectionBackground",
        ],
    ),
    (
        "completions_popup.item_selected_fg",
        &[
            "editorSuggestWidget.selectedForeground",
            "list.activeSelectionForeground",
        ],
    ),
    (
        "completions_popup.item_fg",
        &["editorSuggestWidget.foreground", "foreground"],
    ),
];

/// Strips `//` line comments, `/* */` block comments, and trailing commas before `}`/`]` from
/// `input` - real JSONC noise `serde_json` itself rejects, but genuinely present in real
/// downloaded VSCode theme files. String-aware: a `//` or `,` inside a real JSON string value
/// (an unlikely but real case - a URL in a `"name"` field, say) is left untouched, tracked by
/// toggling an "inside a string" flag on every unescaped `"` and skipping the character right
/// after a `\`.
fn strip_jsonc_noise(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if in_string {
            out.push(ch);
            if ch == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
            }
            ',' => {
                // A trailing comma: everything up to the next `}`/`]` is whitespace or a comment
                // - meaning this comma has nothing real after it in its container. Comments must
                // be skipped here too, not just whitespace: `"x": 1, // trailing` looks past the
                // comment to find the real `}` right after it, rather than stopping at the `/`
                // and wrongly treating the comma as real (a live bug this exact shape caught -
                // the naive whitespace-only lookahead left a real trailing comma in the output
                // once the comment itself was stripped by the rest of this loop).
                let mut j = i + 1;
                loop {
                    while j < chars.len() && chars[j].is_whitespace() {
                        j += 1;
                    }
                    if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'/') {
                        while j < chars.len() && chars[j] != '\n' {
                            j += 1;
                        }
                    } else if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'*') {
                        j += 2;
                        while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '/') {
                            j += 1;
                        }
                        j = (j + 2).min(chars.len());
                    } else {
                        break;
                    }
                }
                if matches!(chars.get(j), Some('}') | Some(']')) {
                    i += 1;
                } else {
                    out.push(ch);
                    i += 1;
                }
            }
            _ => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

/// Normalizes a real VSCode colour string into this app's own `#rrggbb` shape:
/// `#rgb`/`#rgba` shorthand doubled, `#rrggbb` passed through, `#rrggbbaa`'s alpha channel
/// dropped (this app's own theme files carry no alpha - see `custom_theme`'s own module docs on
/// why `#rrggbb` is the one real shape it accepts; VSCode uses translucent colours heavily for
/// selection/diff backgrounds, and taking their hue at full opacity is the honest approximation
/// available). `None` for anything else (a named CSS colour, a malformed string) - VSCode themes
/// overwhelmingly use hex, and guessing at named colours is exactly the kind of "vibe match
/// wearing a precise-looking answer" this module's own docs reject for the load-bearing background
/// field.
fn normalize_vscode_hex(value: &str) -> Option<String> {
    let hex = value.trim().strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        3 | 4 => {
            let doubled: String = hex.chars().take(3).flat_map(|c| [c, c]).collect();
            Some(format!("#{}", doubled.to_ascii_lowercase()))
        }
        6 | 8 => Some(format!("#{}", hex[..6].to_ascii_lowercase())),
        _ => None,
    }
}

fn first_valid_color<'a>(
    colors: &HashMap<String, String>,
    keys: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    keys.into_iter().find_map(|key| {
        colors
            .get(key)
            .and_then(|value| normalize_vscode_hex(value))
    })
}

/// The real colour this theme gives the first of `queries` it says anything about at all.
///
/// `queries` is one bucket's own preference order (most specific textmate scope first - see
/// [`syntax_scope_rule`]), and within a single query the *best* rule wins, not merely the first:
/// the most specific matching selector ([`ScopeField::specificity_for`]), with a later rule
/// beating an earlier one of equal specificity - which is VSCode's own "later `tokenColors`
/// entries override earlier ones" precedence, not a guess.
fn first_scope_foreground(
    token_colors: &[VscodeTokenColorRule],
    queries: &[&str],
) -> Option<String> {
    for query in queries {
        let mut best: Option<(usize, &str)> = None;
        for rule in token_colors {
            let (Some(scope), Some(foreground)) =
                (&rule.scope, rule.settings.foreground.as_deref())
            else {
                continue;
            };
            let Some(specificity) = scope.specificity_for(query) else {
                continue;
            };
            if best.is_none_or(|(best_specificity, _)| specificity >= best_specificity) {
                best = Some((specificity, foreground));
            }
        }
        if let Some(color) = best.and_then(|(_, foreground)| normalize_vscode_hex(foreground)) {
            return Some(color);
        }
    }
    None
}

/// A plausible second swatch when nothing in `colors` names a real sidebar/panel-shaped colour -
/// nudges `background`'s own luma a fixed step in whichever direction gives more room (lighter
/// for a dark theme, darker for a light one), so `panel` is never simply identical to
/// `background` (which would make the derived lightness fit degenerate).
fn derive_panel_fallback(background_hex: &str) -> String {
    let value = u32::from_str_radix(background_hex.trim_start_matches('#'), 16).unwrap_or(0);
    let (r, g, b) = ((value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff);
    let luma = (r * 299 + g * 587 + b * 114) / 1000;
    let shift: i32 = if luma < 128 { 14 } else { -14 };
    let nudge = |channel: u32| (channel as i32 + shift).clamp(0, 255) as u32;
    format!("#{:06x}", (nudge(r) << 16) | (nudge(g) << 8) | nudge(b))
}

/// Whether this theme's own panel colour can be used as the second swatch of the derived base
/// layer's lightness fit (`crate::theme::derive_shift`), or whether a synthesized one
/// ([`derive_panel_fallback`]) has to stand in for it.
///
/// The fit solves a line through `(jerry_bg, theme_bg)` and `(jerry_panel, theme_panel)`, and
/// Jerry Dark's own panel is *lighter* than its window. So:
///
/// - A theme whose window background is dark, like Jerry's, must have its panel lighter than its
///   window for the fit to keep Jerry's own light/dark structure. Plenty of real dark themes
///   (Dracula among them) make their sidebar *darker* than the editor instead - a perfectly good
///   choice for VSCode, but feeding it into the fit solves a **negative** slope, which inverts
///   every token: Jerry's light text would map below zero lightness and clamp to black on a dark
///   background. That is not a subtle quality loss, it is an unusable palette.
/// - A theme whose window background is light *should* invert (that is exactly how the bundled
///   "Paper" theme is derived from Jerry Dark), so there the negative slope is correct and a panel
///   darker than the window is what we want.
///
/// So the usable direction is simply "does this theme's panel sit on the same side of its window
/// as the derivation needs" - lighter for a dark theme, darker for a light one. When it doesn't,
/// [`derive_panel_fallback`] synthesizes one that does, nudging in exactly that direction. The
/// theme's real sidebar colour is not lost by this: it is still mapped directly onto
/// `surface.rail` by [`COLOR_KEY_MAP`], which is the token that actually paints it.
fn panel_direction_is_usable(background_hex: &str, panel_hex: &str) -> bool {
    let luma = |hex: &str| -> i32 {
        let value = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
        let (r, g, b) = ((value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff);
        ((r * 299 + g * 587 + b * 114) / 1000) as i32
    };
    let background = luma(background_hex);
    let panel = luma(panel_hex);
    if background < 128 {
        panel > background
    } else {
        panel < background
    }
}

/// Filename stem, e.g. `dracula-pro.json` -> `"Dracula Pro"` - the real fallback display name
/// when a VSCode theme JSON file names no `"name"` field of its own, which real downloaded theme
/// files very often don't (a VSCode extension's theme *label* usually lives in its
/// `package.json`, not the theme file itself).
fn title_case_from_stem(stem: &str) -> String {
    stem.split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Picks the five representative `[background, panel, green-ish, amber-ish, blue-ish]` colours the
/// whole-app derived base layer is computed from (see the module docs' layer 1). `background` has
/// no default - a theme that names none is a real [`VscodeThemeError::MissingBackground`]; the
/// three accents fall back through the theme's own `tokenColors` and finally to Jerry Dark's own.
fn swatches_from_vscode(file: &VscodeThemeFile) -> Result<[u32; 5], VscodeThemeError> {
    let background = first_valid_color(&file.colors, ["editor.background", "background"])
        .ok_or(VscodeThemeError::MissingBackground)?;
    let panel = first_valid_color(
        &file.colors,
        [
            "sideBar.background",
            "activityBar.background",
            "panel.background",
            "editorGroupHeader.tabsBackground",
        ],
    )
    .filter(|panel| panel_direction_is_usable(&background, panel))
    .unwrap_or_else(|| derive_panel_fallback(&background));
    let accent_green = first_valid_color(
        &file.colors,
        [
            "terminal.ansiGreen",
            "gitDecoration.addedResourceForeground",
            "charts.green",
        ],
    )
    .or_else(|| first_scope_foreground(&file.token_colors, &["string", "markup.inserted"]))
    .unwrap_or_else(|| DEFAULT_ACCENT_GREEN.to_string());
    let accent_amber = first_valid_color(
        &file.colors,
        [
            "terminal.ansiYellow",
            "list.warningForeground",
            "charts.yellow",
        ],
    )
    .or_else(|| first_scope_foreground(&file.token_colors, &["keyword", "support.type"]))
    .unwrap_or_else(|| DEFAULT_ACCENT_AMBER.to_string());
    let accent_blue = first_valid_color(
        &file.colors,
        [
            "button.background",
            "activityBarBadge.background",
            "focusBorder",
            "textLink.foreground",
        ],
    )
    .or_else(|| {
        first_scope_foreground(
            &file.token_colors,
            &["entity.name.function", "support.function"],
        )
    })
    .unwrap_or_else(|| DEFAULT_ACCENT_BLUE.to_string());

    let parse = |value: &str| u32::from_str_radix(value.trim_start_matches('#'), 16).unwrap_or(0);
    Ok([
        parse(&background),
        parse(&panel),
        parse(&accent_green),
        parse(&accent_amber),
        parse(&accent_blue),
    ])
}

/// Converts real VSCode theme JSON `contents` into this app's own [`CustomThemeFile`] shape -
/// **not yet validated**; the caller runs it through `CustomThemeFile::validate` (or
/// [`super::custom_theme::validate_and_write`]'s own validate-then-write path) the same as any
/// other theme file, so a converted-but-unreadable result is rejected the same honest way a
/// hand-authored one would be.
///
/// `source_stem` is the source file's own name without extension (e.g. `"dracula"` for
/// `dracula.json`) - the real fallback display name, see [`title_case_from_stem`].
pub fn convert_vscode_theme_str(
    contents: &str,
    source_stem: &str,
) -> Result<CustomThemeFile, VscodeThemeError> {
    let file = parse_vscode_theme_str(contents)?;
    convert_vscode_theme(file, source_stem)
}

/// The real conversion, over an already-parsed (and already-`include`-resolved) theme.
fn convert_vscode_theme(
    file: VscodeThemeFile,
    source_stem: &str,
) -> Result<CustomThemeFile, VscodeThemeError> {
    let swatches = swatches_from_vscode(&file)?;

    // Layer 1: a real, complete derived base for every token in the app.
    let shift = theme::derive_shift(super::builtin_themes::jerry_dark_swatches(), swatches);
    let mut palette: HashMap<&'static str, gpui::Rgba> =
        theme::derived_palette(shift).into_iter().collect();

    // Layer 2: every key this theme really names, on top.
    for (token_key, vscode_keys) in COLOR_KEY_MAP {
        let Some(color) = first_valid_color(&file.colors, vscode_keys.iter().copied()) else {
            continue;
        };
        let Some(token) = theme::token_for_key(token_key) else {
            debug_assert!(false, "{token_key} is not a real registered theme token");
            continue;
        };
        if let Some(hex) = parse_normalized_hex(&color) {
            palette.insert(token.key, theme::hex_rgba(hex));
        }
    }
    for (token_key, color) in build_syntax_overrides(&file.colors, &file.token_colors) {
        if let (Some(token), Some(hex)) = (
            theme::token_for_key(&token_key),
            parse_normalized_hex(&color),
        ) {
            palette.insert(token.key, theme::hex_rgba(hex));
        }
    }

    let name = file
        .name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| title_case_from_stem(source_stem));

    Ok(super::builtin_themes::generated_theme_file(
        &name,
        "imported from a VSCode theme",
        swatches,
        theme::all_tokens()
            .filter_map(|token| palette.get(token.key).map(|color| (token.key, *color)))
            .collect(),
    ))
}

/// A `#rrggbb` string [`normalize_vscode_hex`] already produced -> its `0xrrggbb` value.
fn parse_normalized_hex(value: &str) -> Option<u32> {
    u32::from_str_radix(value.strip_prefix('#')?, 16).ok()
}

/// One [`HighlightKind`]'s real, ordered list of textmate scope prefixes to search
/// [`file.token_colors`](VscodeThemeFile::token_colors) for, plus the kind to inherit its colour
/// from when no rule matches - the real bucket dependency order [`crate::theme::syntax`]'s own
/// module docs already establish as this app's *default* fallback chain (`FUNCTION_METHOD` after
/// `FUNCTION`, `TYPE_BUILTIN` after `TYPE`, `VARIABLE` after `TEXT`, ...), applied here at import
/// time, so a VSCode theme that only styles `entity.name.function` still gets a real, consistent
/// colour for a method call too - not a jarring mix of "some tokens follow the theme, some
/// silently don't".
///
/// A `None` parent means "this bucket has no ancestor to inherit from" - the same real roots
/// `theme::syntax` itself uses (`KEYWORD`/`FUNCTION`/`TYPE`/`CONSTANT`/`STRING`/`COMMENT`/
/// `ATTRIBUTE`/`STRONG`/`EMPHASIS` are independently authored hues, not defaults borrowed from
/// another bucket).
fn syntax_scope_rule(kind: HighlightKind) -> (&'static [&'static str], Option<HighlightKind>) {
    use HighlightKind::*;
    match kind {
        Keyword => (
            &[
                "keyword.control",
                "keyword.other",
                "storage.modifier",
                "keyword",
            ],
            None,
        ),
        Function => (&["entity.name.function", "support.function"], None),
        FunctionMethod => (
            &[
                "entity.name.function.method",
                "support.function.method",
                "meta.function-call entity.name.function",
            ],
            Some(Function),
        ),
        Type => (
            &["entity.name.type", "entity.name.class", "support.class"],
            None,
        ),
        TypeBuiltin => (
            &["support.type", "storage.type.primitive", "keyword.type"],
            Some(Type),
        ),
        Constant => (&["constant.other", "constant"], None),
        ConstantBuiltin => (&["constant.language", "keyword.constant"], Some(Constant)),
        String => (&["string.quoted", "string"], None),
        StringEscape => (&["constant.character.escape"], Some(String)),
        Number => (&["constant.numeric"], Some(Constant)),
        Comment => (&["comment.line", "comment.block", "comment"], None),
        CommentDoc => (
            &["comment.block.documentation", "comment.documentation"],
            Some(Comment),
        ),
        Variable => (&["variable.other.readwrite", "variable"], Some(Text)),
        VariableParameter => (&["variable.parameter"], Some(Variable)),
        VariableBuiltin => (
            &[
                "variable.language",
                "variable.language.this",
                "keyword.other.this",
            ],
            Some(Constant),
        ),
        Property => (
            &[
                "variable.other.property",
                "variable.other.object.property",
                "meta.object-literal.key",
            ],
            Some(Variable),
        ),
        Operator => (&["keyword.operator"], Some(Text)),
        PunctuationBracket => (
            &[
                "punctuation.section.brackets",
                "punctuation.definition.brace",
                "punctuation.section.parens",
            ],
            Some(Text),
        ),
        PunctuationDelimiter => (
            &[
                "punctuation.separator",
                "punctuation.terminator",
                "punctuation.delimiter",
            ],
            Some(Text),
        ),
        Tag => (&["entity.name.tag"], Some(Type)),
        Attribute => (&["entity.other.attribute-name", "storage.modifier"], None),
        Embedded => (&[], Some(Text)),
        Text => (&[], None),
        Heading => (&["markup.heading", "entity.name.section"], Some(Type)),
        Link => (
            &["markup.underline.link", "string.other.link"],
            Some(Function),
        ),
        Strong => (&["markup.bold"], None),
        Emphasis => (&["markup.italic"], None),
    }
}

/// Builds the real `syntax.*` half of a converted palette: every [`HighlightKind`] this VSCode
/// theme's own `colors`/`tokenColors` give a real, resolved colour for, keyed by the matching
/// `crate::theme::syntax` token key (`HighlightKind::name` and that token's key are the same
/// snake_case string by construction - see [`tests::every_highlight_kind_maps_onto_a_real_syntax_token`],
/// which proves it rather than assuming it).
///
/// Processed in [`HighlightKind::ALL`]'s own declaration order (parents before every child that
/// can inherit from them - see [`syntax_scope_rule`]) so a child's fallback always reads an
/// already-resolved parent, never one that hasn't been visited yet.
///
/// [`HighlightKind::Text`] is the one real special case: it comes from `colors["editor.foreground"]`
/// (a real VSCode UI colour, not a `tokenColors` scope - there is no textmate scope for "plain
/// text with no other rule matching") rather than a scope search.
fn build_syntax_overrides(
    colors: &HashMap<String, String>,
    token_colors: &[VscodeTokenColorRule],
) -> Vec<(String, String)> {
    let mut resolved: HashMap<HighlightKind, String> = HashMap::new();
    if let Some(foreground) = first_valid_color(colors, ["editor.foreground", "foreground"]) {
        resolved.insert(HighlightKind::Text, foreground);
    }
    for kind in HighlightKind::ALL {
        if kind == HighlightKind::Text {
            continue;
        }
        let (scopes, parent) = syntax_scope_rule(kind);
        let color = first_scope_foreground(token_colors, scopes)
            .or_else(|| parent.and_then(|parent| resolved.get(&parent).cloned()));
        if let Some(color) = color {
            resolved.insert(kind, color);
        }
    }
    // Registry order, not `HashMap` order, so a converted file's `[syntax]` table comes out
    // deterministic rather than differing between two imports of the same source file.
    HighlightKind::ALL
        .into_iter()
        .filter_map(|kind| {
            resolved
                .get(&kind)
                .map(|color| (format!("syntax.{}", kind.name()), color.clone()))
        })
        .collect()
}

/// Reads and converts `source_path` - the real file-path entry point behind
/// [`import_vscode_theme_file`]. Hands back an *unvalidated* [`CustomThemeFile`]; the caller still
/// runs the shared validate-then-write path.
pub fn convert_vscode_theme_file(source_path: &Path) -> Result<CustomThemeFile, VscodeThemeError> {
    let file = load_vscode_theme_with_includes(source_path, 0)?;
    let stem = source_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    convert_vscode_theme(file, &stem)
}

/// Every real, specific way importing a VSCode theme *file* (as opposed to just converting
/// already-read text, [`convert_vscode_theme_str`]'s own concern) can fail - a real conversion
/// error, or the *converted* result failing the shared validate/write pipeline every other theme
/// import goes through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VscodeImportError {
    Convert(VscodeThemeError),
    Theme(super::custom_theme::ThemeFileError),
}

impl std::fmt::Display for VscodeImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VscodeImportError::Convert(err) => write!(f, "{err}"),
            VscodeImportError::Theme(err) => write!(f, "{err}"),
        }
    }
}

/// The real file-path entry point `crate::settings::render::AdeApp::start_import_vscode_theme`
/// uses: reads and converts `source_path`, then runs the result through the exact same
/// validate-then-write-a-canonical-copy path [`super::custom_theme::import_theme_file`] uses for
/// a plain TOML theme, so an imported VSCode theme is a real, indistinguishable
/// [`super::custom_theme::CustomTheme`] afterward - same readability floor, same collision-safe
/// destination naming, same re-import-updates-in-place behaviour, and a real, hand-editable file.
pub fn import_vscode_theme_file(
    source_path: &Path,
    dest_dir: &Path,
) -> Result<super::custom_theme::CustomTheme, VscodeImportError> {
    let file = convert_vscode_theme_file(source_path).map_err(VscodeImportError::Convert)?;
    super::custom_theme::validate_and_write(file, dest_dir).map_err(VscodeImportError::Theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The converted file's real entry for `key`, as a `#rrggbb` string.
    fn entry(file: &CustomThemeFile, key: &str) -> Option<String> {
        file.overrides
            .iter()
            .find(|(entry_key, _)| entry_key == key)
            .map(|(_, value)| value.clone())
    }

    #[test]
    fn strip_jsonc_noise_removes_line_and_block_comments_and_trailing_commas() {
        let input = r#"{
            // a line comment
            "a": 1, /* a block
                       comment */
            "b": 2,
        }"#;
        let stripped = strip_jsonc_noise(input);
        let parsed: serde_json::Value = serde_json::from_str(&stripped).expect("valid json");
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn strip_jsonc_noise_leaves_a_double_slash_inside_a_real_string_untouched() {
        let input = r#"{"name": "https://example.com/theme"}"#;
        let stripped = strip_jsonc_noise(input);
        let parsed: serde_json::Value = serde_json::from_str(&stripped).expect("valid json");
        assert_eq!(parsed["name"], "https://example.com/theme");
    }

    #[test]
    fn normalize_vscode_hex_handles_every_real_shape() {
        assert_eq!(normalize_vscode_hex("#1a2b3c"), Some("#1a2b3c".to_string()));
        assert_eq!(
            normalize_vscode_hex("#1A2B3C"),
            Some("#1a2b3c".to_string()),
            "must lowercase"
        );
        assert_eq!(
            normalize_vscode_hex("#1a2b3cff"),
            Some("#1a2b3c".to_string()),
            "must drop the alpha channel"
        );
        assert_eq!(
            normalize_vscode_hex("#abc"),
            Some("#aabbcc".to_string()),
            "must double shorthand digits"
        );
        assert_eq!(normalize_vscode_hex("#abcd"), Some("#aabbcc".to_string()));
        assert_eq!(normalize_vscode_hex("not-a-color"), None);
        assert_eq!(
            normalize_vscode_hex("#12345"),
            None,
            "5 digits is not a real shape"
        );
    }

    /// Every VSCode key this module claims to map must target a *real* registered token - a typo
    /// here would silently drop that mapping, and `debug_assert!` alone wouldn't be checked in a
    /// release build.
    #[test]
    fn every_mapped_token_key_is_a_real_registered_theme_token() {
        for (token_key, vscode_keys) in COLOR_KEY_MAP {
            assert!(
                theme::token_for_key(token_key).is_some(),
                "{token_key} is not a real registered theme token"
            );
            assert!(
                !vscode_keys.is_empty(),
                "{token_key} maps to no VSCode key at all"
            );
        }
    }

    /// The `syntax.*` half depends on `HighlightKind::name()` and the matching token key being the
    /// same string - proven here rather than assumed.
    #[test]
    fn every_highlight_kind_maps_onto_a_real_syntax_token() {
        for kind in HighlightKind::ALL {
            let key = format!("syntax.{}", kind.name());
            assert!(
                theme::token_for_key(&key).is_some(),
                "{key} (from HighlightKind::{kind:?}) is not a real registered token"
            );
        }
    }

    #[test]
    fn a_real_minimal_theme_converts_into_a_complete_derived_palette() {
        let json = r##"{
            "name": "Sample Theme",
            "colors": { "editor.background": "#101214" }
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(file.name, "Sample Theme");
        assert_eq!(file.base.as_deref(), Some("Jerry Dark"));
        assert_eq!(
            file.overrides.len(),
            theme::all_tokens().count(),
            "an imported theme must be a complete palette, not a handful of keys - every token \
             the theme itself doesn't name still gets a real derived colour in its own family"
        );
        assert_eq!(entry(&file, "surface.window").as_deref(), Some("#101214"));
    }

    #[test]
    fn real_colors_keys_win_over_the_derived_base_layer() {
        let json = r##"{
            "colors": {
                "editor.background": "#101214",
                "sideBar.background": "#1a1e21",
                "statusBar.background": "#2b2d3a",
                "terminal.ansiGreen": "#50fa7b",
                "terminal.ansiRed": "#ff5555",
                "editorCursor.foreground": "#f8f8f0",
                "list.hoverBackground": "#313442"
            }
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(entry(&file, "surface.rail").as_deref(), Some("#1a1e21"));
        assert_eq!(entry(&file, "surface.footer").as_deref(), Some("#2b2d3a"));
        assert_eq!(entry(&file, "status.review").as_deref(), Some("#50fa7b"));
        assert_eq!(entry(&file, "term.ok").as_deref(), Some("#50fa7b"));
        assert_eq!(entry(&file, "status.fail").as_deref(), Some("#ff5555"));
        assert_eq!(entry(&file, "editor.caret").as_deref(), Some("#f8f8f0"));
        assert_eq!(
            entry(&file, "surface.row_hover").as_deref(),
            Some("#313442")
        );
    }

    /// The real breadth claim: a theme with a normal VSCode colour set moves genuinely many
    /// distinct Jerry tokens to its *own literal* values, across unrelated modules - not just the
    /// three or four the old five-swatch conversion could.
    #[test]
    fn a_realistic_theme_maps_many_real_keys_across_unrelated_modules() {
        let json = r##"{
            "name": "Broad",
            "colors": {
                "editor.background": "#282a36",
                "editor.foreground": "#f8f8f2",
                "sideBar.background": "#21222c",
                "activityBar.background": "#343746",
                "statusBar.background": "#191a21",
                "titleBar.activeBackground": "#21222c",
                "editorWidget.background": "#21222c",
                "input.background": "#282a36",
                "list.hoverBackground": "#44475a",
                "list.activeSelectionBackground": "#44475a",
                "editor.lineHighlightBackground": "#44475a",
                "editor.selectionBackground": "#44475a",
                "editorCursor.foreground": "#f8f8f0",
                "editorLineNumber.foreground": "#6272a4",
                "editorLineNumber.activeForeground": "#f8f8f2",
                "editorIndentGuide.background1": "#424450",
                "editorWhitespace.foreground": "#424450",
                "editorError.foreground": "#ff5555",
                "editorWarning.foreground": "#ffb86c",
                "editorGutter.addedBackground": "#50fa7b",
                "editorGutter.deletedBackground": "#ff5555",
                "editorGutter.modifiedBackground": "#8be9fd",
                "gitDecoration.addedResourceForeground": "#50fa7b",
                "gitDecoration.deletedResourceForeground": "#ff5555",
                "diffEditor.insertedTextBackground": "#50fa7b33",
                "diffEditor.removedTextBackground": "#ff555533",
                "terminal.foreground": "#f8f8f2",
                "terminal.ansiGreen": "#50fa7b",
                "terminal.ansiRed": "#ff5555",
                "terminal.ansiYellow": "#f1fa8c",
                "terminal.ansiBlue": "#bd93f9",
                "terminal.ansiBrightBlack": "#6272a4",
                "terminal.ansiBrightWhite": "#ffffff",
                "button.background": "#44475a",
                "button.foreground": "#f8f8f2",
                "badge.background": "#44475a",
                "focusBorder": "#6272a4",
                "foreground": "#f8f8f2",
                "descriptionForeground": "#6272a4",
                "scrollbarSlider.background": "#44475a",
                "textLink.foreground": "#8be9fd"
            },
            "tokenColors": [
                { "scope": "keyword", "settings": { "foreground": "#ff79c6" } },
                { "scope": "string", "settings": { "foreground": "#f1fa8c" } },
                { "scope": "comment", "settings": { "foreground": "#6272a4" } },
                { "scope": "entity.name.function", "settings": { "foreground": "#50fa7b" } },
                { "scope": "entity.name.type", "settings": { "foreground": "#8be9fd" } },
                { "scope": "variable.parameter", "settings": { "foreground": "#ffb86c" } }
            ]
        }"##;
        let file = convert_vscode_theme_str(json, "broad").expect("convert");

        // Every value the theme literally names is present verbatim, spread across modules that
        // have nothing to do with each other.
        for (key, expected) in [
            ("surface.window", "#282a36"),
            ("surface.rail", "#21222c"),
            ("surface.footer", "#191a21"),
            ("surface.title_bar", "#21222c"),
            ("text.body", "#f8f8f2"),
            ("text.muted", "#6272a4"),
            ("status.fail", "#ff5555"),
            ("status.ask", "#f1fa8c"),
            ("editor.gutter_text", "#6272a4"),
            ("editor.whitespace", "#424450"),
            ("term.dim", "#6272a4"),
            ("term.heading", "#ffffff"),
            ("diff.git_gutter", "#8be9fd"),
            ("scrollbar.thumb", "#44475a"),
            ("button.blue_bg", "#44475a"),
            ("syntax.keyword", "#ff79c6"),
            ("syntax.string", "#f1fa8c"),
            ("syntax.comment", "#6272a4"),
            ("syntax.function", "#50fa7b"),
            ("syntax.type", "#8be9fd"),
            ("syntax.variable_parameter", "#ffb86c"),
        ] {
            assert_eq!(
                entry(&file, key).as_deref(),
                Some(expected),
                "{key} should have come straight from the VSCode theme"
            );
        }

        // A translucent VSCode colour keeps its hue at full opacity.
        assert_eq!(entry(&file, "diff.add_bg").as_deref(), Some("#50fa7b"));

        // And the whole thing is still a real, valid, selectable theme.
        let validated = file
            .validate()
            .expect("must pass the shared validate pipeline");
        assert_eq!(validated.name, "Broad");
    }

    /// A former Rust-level alias pair really can diverge now: `variable.parameter` is styled by
    /// this theme, `variable` isn't, and the two must land on genuinely different colours - the
    /// concrete fidelity win the token rewrite bought for VSCode import.
    #[test]
    fn a_child_scope_the_theme_styles_diverges_from_the_parent_it_used_to_alias() {
        let json = r##"{
            "colors": { "editor.background": "#101214", "editor.foreground": "#eeeeee" },
            "tokenColors": [
                { "scope": "variable.parameter", "settings": { "foreground": "#ffb86c" } }
            ]
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(
            entry(&file, "syntax.variable_parameter").as_deref(),
            Some("#ffb86c")
        );
        assert_eq!(entry(&file, "syntax.variable").as_deref(), Some("#eeeeee"));
        assert_ne!(
            entry(&file, "syntax.variable_parameter"),
            entry(&file, "syntax.variable"),
            "before the token rewrite these were the same const and could not differ at all"
        );
    }

    #[test]
    fn token_colors_are_a_real_fallback_when_the_colors_map_names_no_accent() {
        let json = r##"{
            "colors": { "editor.background": "#101214" },
            "tokenColors": [
                { "scope": "string.quoted", "settings": { "foreground": "#7fd88f" } },
                { "scope": ["keyword.control", "keyword.other"], "settings": { "foreground": "#e9c46a" } }
            ]
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(entry(&file, "syntax.string").as_deref(), Some("#7fd88f"));
        assert_eq!(entry(&file, "syntax.keyword").as_deref(), Some("#e9c46a"));
    }

    #[test]
    fn missing_editor_background_is_a_real_error_not_a_silent_default() {
        let json = r#"{ "colors": {} }"#;
        assert_eq!(
            convert_vscode_theme_str(json, "sample"),
            Err(VscodeThemeError::MissingBackground)
        );
    }

    #[test]
    fn a_top_level_background_key_is_a_real_fallback_for_editor_background() {
        let json = r##"{ "colors": { "background": "#0a0b0c" } }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(entry(&file, "surface.window").as_deref(), Some("#0a0b0c"));
    }

    #[test]
    fn a_theme_with_no_name_field_falls_back_to_the_real_source_filename() {
        let json = r##"{ "colors": { "editor.background": "#101214" } }"##;
        let file = convert_vscode_theme_str(json, "dracula-pro").expect("convert");
        assert_eq!(file.name, "Dracula Pro");
    }

    #[test]
    fn malformed_json_is_a_real_parse_error() {
        let result = convert_vscode_theme_str("{ not json", "sample");
        assert!(matches!(result, Err(VscodeThemeError::Parse(_))));
    }

    #[test]
    fn a_real_jsonc_file_with_comments_and_trailing_commas_still_converts() {
        let json = r##"{
            // real theme
            "name": "Commented",
            "colors": {
                "editor.background": "#101214", // dark bg
            },
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(file.name, "Commented");
        assert_eq!(entry(&file, "surface.window").as_deref(), Some("#101214"));
    }

    #[test]
    fn the_converted_file_passes_the_real_shared_validate_pipeline() {
        let json = r##"{
            "name": "Round Trip Theme",
            "colors": {
                "editor.background": "#0c0d10",
                "sideBar.background": "#181a1e",
                "terminal.ansiGreen": "#5cb87f",
                "terminal.ansiYellow": "#e2a336",
                "button.background": "#e07a5f"
            }
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        let validated = file
            .validate()
            .expect("must pass the shared validate() pipeline");
        assert_eq!(validated.name, "Round Trip Theme");
    }

    #[test]
    fn a_child_bucket_with_no_direct_scope_match_inherits_its_real_parents_resolved_colour() {
        // No "entity.name.function.method"/"support.function.method" rule at all - only the
        // parent "entity.name.function" is styled. FunctionMethod must still resolve, to the
        // same colour, rather than being left on the derived value.
        let json = r##"{
            "colors": { "editor.background": "#101214" },
            "tokenColors": [
                { "scope": "entity.name.function", "settings": { "foreground": "#8be9fd" } }
            ]
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(entry(&file, "syntax.function").as_deref(), Some("#8be9fd"));
        assert_eq!(
            entry(&file, "syntax.function_method").as_deref(),
            Some("#8be9fd"),
            "FunctionMethod has no direct scope match here, so it must inherit Function's own \
             real resolved colour"
        );
    }

    #[test]
    fn a_direct_child_scope_match_wins_over_inheriting_the_parent() {
        let json = r##"{
            "colors": { "editor.background": "#101214" },
            "tokenColors": [
                { "scope": "entity.name.function", "settings": { "foreground": "#8be9fd" } },
                { "scope": "entity.name.function.method", "settings": { "foreground": "#50fa7b" } }
            ]
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(
            entry(&file, "syntax.function_method").as_deref(),
            Some("#50fa7b"),
            "a real, direct scope match for the child bucket must win over inheriting its parent"
        );
    }

    #[test]
    fn editor_foreground_becomes_the_real_text_syntax_colour() {
        let json = r##"{
            "colors": {
                "editor.background": "#101214",
                "editor.foreground": "#f8f8f2"
            }
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(entry(&file, "syntax.text").as_deref(), Some("#f8f8f2"));
    }

    /// A converted theme really round-trips through this app's own file format - the property
    /// `import_vscode_theme_file`'s write-then-reload depends on.
    #[test]
    fn a_converted_theme_round_trips_through_the_real_toml_writer_and_parser() {
        let json = r##"{
            "name": "Round Trip",
            "colors": { "editor.background": "#282a36", "sideBar.background": "#21222c" },
            "tokenColors": [
                { "scope": "keyword", "settings": { "foreground": "#ff79c6" } }
            ]
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        let validated = file.validate().expect("must validate");
        let reparsed =
            super::super::custom_theme::parse_theme_file_str(&validated.to_toml_string())
                .expect("the written file must re-parse");
        assert_eq!(reparsed.overrides, validated.overrides);
        assert_eq!(reparsed.preview, validated.preview);
    }
}

/// Real end-to-end import coverage against **actual, unmodified VSCode theme files** - not
/// synthetic fixtures that might miss the paths a real file exercises.
///
/// `testdata/vscode/*.json` are the genuine files from Microsoft's own `theme-defaults` extension
/// (MIT licensed, vendored verbatim), including their real JSONC comments, tabs, and `include`
/// chains. Two real, user-reported bugs were found by pointing this at them rather than at a
/// hand-written fixture:
///
/// - `Dark+` (`dark_plus.json`) failed to convert at all: it defines *no* `colors`, only
///   `tokenColors` plus `"include": "./dark_vs.json"`, so ignoring `include` meant the converter
///   genuinely could not see a background.
/// - `Dark Modern` (`dark_modern.json`) converted but was then rejected by this crate's own
///   readability check, because it sets `editor.background` `#1F1F1F` and
///   `editorWidget.background` `#202020` - a deliberate flat-surface design that the old
///   surface-against-surface check mistook for an unreadable theme.
#[cfg(test)]
mod real_vscode_default_theme_tests {
    use super::*;
    use crate::settings::custom_theme;

    /// Writes the whole vendored fixture directory into a temp dir, so an `include` chain resolves
    /// against real files on disk exactly as it does for a user importing from their VSCode
    /// installation.
    fn real_theme_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, contents) in [
            ("dark_vs.json", include_str!("testdata/vscode/dark_vs.json")),
            (
                "dark_plus.json",
                include_str!("testdata/vscode/dark_plus.json"),
            ),
            (
                "dark_modern.json",
                include_str!("testdata/vscode/dark_modern.json"),
            ),
            (
                "light_vs.json",
                include_str!("testdata/vscode/light_vs.json"),
            ),
            (
                "light_plus.json",
                include_str!("testdata/vscode/light_plus.json"),
            ),
            (
                "light_modern.json",
                include_str!("testdata/vscode/light_modern.json"),
            ),
            // Three more real, widely-used themes, for breadth beyond the shipped defaults:
            // Monokai and Solarized Dark from VSCode's own bundled extensions, and One Dark Pro
            // (MIT, the most-installed theme on the marketplace) - all verbatim, none hand-made.
            ("monokai.json", include_str!("testdata/vscode/monokai.json")),
            (
                "solarized_dark.json",
                include_str!("testdata/vscode/solarized_dark.json"),
            ),
            ("onedark.json", include_str!("testdata/vscode/onedark.json")),
        ] {
            std::fs::write(dir.path().join(name), contents).expect("write fixture");
        }
        dir
    }

    /// The non-negotiable one: every real, shipped VSCode default theme imports end to end -
    /// converted, validated, written to disk, and loadable back as a real selectable theme.
    #[test]
    fn every_real_vscode_default_theme_imports_end_to_end() {
        let source_dir = real_theme_dir();
        let dest_dir = tempfile::tempdir().expect("tempdir");

        for file_name in [
            "dark_vs.json",
            "dark_plus.json",
            "dark_modern.json",
            "light_vs.json",
            "light_plus.json",
            "light_modern.json",
            "monokai.json",
            "solarized_dark.json",
            "onedark.json",
        ] {
            let imported =
                import_vscode_theme_file(&source_dir.path().join(file_name), dest_dir.path())
                    .unwrap_or_else(|err| {
                        panic!("{file_name} is a real shipped VSCode theme and must import: {err}")
                    });
            assert_eq!(
                imported.overrides.len(),
                theme::all_tokens().count(),
                "{file_name} must import as a real, complete palette"
            );
            let path = imported
                .source_path
                .as_ref()
                .expect("an imported theme has a real backing file");
            assert!(path.exists(), "{file_name} must really land on disk");
        }

        // And every one of them loads back cleanly from the directory it was written into.
        let (loaded, errors) = custom_theme::load_custom_themes_from_dir(dest_dir.path());
        assert!(
            errors.is_empty(),
            "re-loading the imports reported: {errors:?}"
        );
        assert_eq!(loaded.len(), 9, "every imported theme must load back");
    }

    /// The `include` chain is really followed, not merely tolerated: `Dark+` carries no `colors`
    /// of its own at all, so every chrome colour in the result has to have come from
    /// `dark_vs.json`, while its *own* `tokenColors` still win for syntax.
    #[test]
    fn dark_plus_really_inherits_its_colours_through_the_include_chain() {
        let source_dir = real_theme_dir();
        let raw = include_str!("testdata/vscode/dark_plus.json");
        let parsed = parse_vscode_theme_str(raw).expect("the real file must parse");
        assert!(
            parsed.colors.is_empty(),
            "premise: the real Dark+ file defines no colors of its own"
        );
        assert_eq!(parsed.include.as_deref(), Some("./dark_vs.json"));
        assert!(
            convert_vscode_theme_str(raw, "dark_plus").is_err(),
            "premise: without following the include there is nothing to convert"
        );

        let file = convert_vscode_theme_file(&source_dir.path().join("dark_plus.json"))
            .expect("following the include must make this convertible");
        let entry = |key: &str| {
            file.overrides
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(file.name, "Dark+");
        assert_eq!(
            entry("surface.window").as_deref(),
            Some("#1e1e1e"),
            "the window background has to have come from dark_vs.json's editor.background"
        );
        // Dark+'s own tokenColors style function declarations; that must still win.
        assert_eq!(entry("syntax.function").as_deref(), Some("#dcdcaa"));
    }

    /// A circular `include` is a real, bounded error rather than a hang or a stack overflow.
    #[test]
    fn a_circular_include_chain_is_a_real_reported_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("a.json"),
            r#"{ "include": "./b.json", "colors": {} }"#,
        )
        .expect("write");
        std::fs::write(
            dir.path().join("b.json"),
            r#"{ "include": "./a.json", "colors": {} }"#,
        )
        .expect("write");

        let err = convert_vscode_theme_file(&dir.path().join("a.json")).unwrap_err();
        assert_eq!(
            err,
            VscodeThemeError::IncludeTooDeep {
                limit: MAX_INCLUDE_DEPTH
            }
        );
    }

    /// A missing include target is reported honestly rather than silently importing a theme with
    /// none of the colours the user expected.
    #[test]
    fn a_missing_include_target_is_a_real_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("orphan.json"),
            r#"{ "include": "./nope.json", "colors": {} }"#,
        )
        .expect("write");
        assert!(matches!(
            convert_vscode_theme_file(&dir.path().join("orphan.json")),
            Err(VscodeThemeError::Parse(_))
        ));
    }

    /// The light defaults really import as light themes, and the dark ones as dark - a real check
    /// that the whole conversion preserved the source theme's own character rather than merely
    /// producing *something* valid.
    #[test]
    fn the_real_light_defaults_import_as_light_themes_and_the_dark_ones_as_dark() {
        let source_dir = real_theme_dir();
        for (file_name, expect_light) in [
            ("dark_vs.json", false),
            ("dark_modern.json", false),
            ("light_vs.json", true),
            ("light_modern.json", true),
        ] {
            let file = convert_vscode_theme_file(&source_dir.path().join(file_name))
                .expect("must convert");
            let theme = file.validate().expect("must validate");
            assert_eq!(
                theme::theme_is_light(theme.window_background()),
                expect_light,
                "{file_name} imported with the wrong light/dark character"
            );
        }
    }
}
