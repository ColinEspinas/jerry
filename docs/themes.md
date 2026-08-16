# Themes

Jerry's entire interface is painted from about 270 named color tokens (`crates/app/src/theme.rs`),
grouped into modules — `surface`, `border`, `text`, `status`, `syntax`, `term`, `diff`, `editor`,
`graph`, and so on. A **theme** is a file that names any subset of those tokens; everything it
doesn't name is inherited from the theme it declares as its `base`, and ultimately from Jerry
Dark's own compiled-in defaults.

For the design rationale behind the syntax-highlighting tier specifically (why particular hues were
chosen, the contrast floors, the bracket-pair depth ring), see
[`theme-palette-design.md`](./theme-palette-design.md). This document is the file format and the
authoring workflow instead.

There are six built-in themes (Jerry Dark, Jerry Dim, Slate, Ember, Moss, Paper) and any number of
user-authored ones (GitHub issue #5). Both kinds are shown as the same kind of card on Settings →
Themes, and both are literally the same kind of file: the built-ins live at `assets/themes/*.toml`
in this repository, embedded into the binary and parsed through the exact same code a custom
theme's own file goes through. Jerry Dark's own file names no colors at all — it *is* the compiled
default palette; the other five are complete, literal, hand-editable palettes.

## File format

One `.toml` file per theme. Jerry writes them with section headings and a comment on most keys —
the comments are pulled from the color tokens' own doc comments in `crates/app/src/theme.rs`, so
they can't drift from what the code says — and reads them liberally: key order, grouping and
comments carry no meaning, so a hand-edited file never has to look like a generated one.

```toml
name = "Midnight Coral"
subtitle = "warm accent, dark base"
base = "Jerry Dark"

# The five swatches this theme's card shows on the Themes page.
preview = ["#0c0d10", "#101216", "#5cb87f", "#e2a336", "#e07a5f"]

[surface]
window       = "#0c0d10"  # window body
rail         = "#101216"  # left rail + right panel
card         = "#181a1e"  # composer, settings cards
row_selected = "#1b1f26"

[syntax]
keyword  = "#ff79c6"
string   = "#f1fa8c"
variable = "#bd89a5"
```

- **`name`** — required, non-empty, and must not reuse a built-in theme's name.
- **`subtitle`** — optional one-line description shown on the card.
- **`base`** — optional; the theme every unnamed key is inherited from. `"Jerry Dark"` is the usual
  choice, and omitting it is equivalent. A `base` chain that loops is rejected with a real error
  naming the whole chain.
- **`preview`** — optional array of five `#rrggbb` colors for the card's swatch strip. Omitted, it
  is read from the theme's own `surface.window`/`surface.rail`/`status.review`/`status.ask`/
  `status.run`.
- **every other table** is a `crate::theme` module, and every key in it is one of that module's
  tokens with its Rust constant name lowercased: `theme::surface::WINDOW` is `[surface] window`,
  `theme::syntax::FUNCTION_METHOD` is `[syntax] function_method`. Pair and array tokens use a
  quoted dotted key inside their table (`"sonnet.fg"`, `"lanes.0"`).

Colors are `#rrggbb` — a `#` plus exactly six hex digits; no `#rgb` shorthand, alpha channel, or
named CSS colors. An unknown table or key is a real, specific rejection naming what it didn't
recognize, never a silently ignored typo.

A theme naming three keys is a complete, valid theme, and stays valid as Jerry grows: keys added by
future versions simply inherit, so a file never has to be kept exhaustive. Deleting a line is a
real, supported edit — that key goes back to what it inherited.

The one thing Jerry insists on is that text is legible: if body text or code would be effectively
invisible against the surface behind it (below 1.6:1 contrast), the theme is rejected with an error
saying which pair failed. Nothing else about a palette is second-guessed — flat designs that
separate regions with borders rather than brightness (VSCode's own Dark Modern, for one) are
perfectly fine.

For the full list of real keys, open any bundled theme —
[`assets/themes/slate.toml`](../assets/themes/slate.toml) and its siblings are complete, commented
palettes, and copying one is the fastest way to author a whole theme.
[`assets/themes/template.toml`](../assets/themes/template.toml) is a smaller commented starting
point.

**Where files live.** `~/.config/jerry/themes/*.toml` — a `themes` directory sitting next to
`~/.config/jerry/settings.toml`. Every `.toml` file directly inside it is loaded as a theme at
startup; a file that fails to parse, validate, or resolve its `base` is skipped with a real,
specific error shown on the Themes page (the rest of the directory still loads normally).

## Authoring without leaving the app

The Themes page's "Custom themes" section has five actions: **New from template…** writes the
commented starting-point file above straight into that directory; **Import theme…** validates and
copies in any `.toml` file you already have, via a native file picker; **Import VSCode theme…**
converts a downloaded VSCode theme `.json` file (see below) the same way; **Generate from color…**
takes one hex color and derives a whole theme from it (see below); **Export current theme…** saves
whichever theme is currently active to a file you can hand to someone else. Every custom theme card
also has a two-click **Remove** action that deletes its backing file.

**Generate from color.** Type a `#rrggbb` seed into the Themes page's own input and click Generate:
Jerry rotates its whole palette so its accent blue lands on that hue, scales saturation to match,
leaves lightness alone (so the theme's light/dark structure survives), and writes the result out as
a complete, literal theme file — all ~270 keys, ready to hand-tune line by line. This is the same
HSL derivation (`derive_shift`/`apply_shift` in `crates/app/src/theme.rs`) that used to compute
every non-Jerry-Dark color live on every render; it is now strictly an authoring tool that produces
files, never part of live rendering.

**Importing a VSCode theme (GitHub issue #141).** "Import VSCode theme…" picks a VSCode theme JSON
file (JSONC — `//`/`/* */` comments and trailing commas are tolerated, since that's how most
downloaded theme files are actually written) and converts it into a Jerry theme file, in two
layers:

- **A complete derived base.** Five representative colors (`editor.background`, a sidebar/panel
  background, and three accents from keys like `terminal.ansiGreen`/`terminal.ansiYellow`/
  `button.background`) are run through the same derivation "Generate from color" uses, giving every
  one of Jerry's ~270 tokens a value in the theme's own family. This is what stops an imported
  light theme from leaving half the chrome dark. VSCode's own default themes are defined as deltas
  on each other (`Dark+` is `tokenColors` plus `"include": "./dark_vs.json"`, and `Dark Modern`
  includes *that*), so the importer follows an `include` chain relative to the file's own
  directory, with the including file winning on `colors` and its `tokenColors` appended after the
  base's. Every shipped VSCode default — Dark+/Light+, Dark/Light Modern, the `_vs` bases — plus
  Monokai, Solarized Dark and One Dark Pro is imported end-to-end by this crate's own tests, against
  the real, unmodified upstream JSON.
- **Every directly-mapped key on top.** Jerry's tokens are mapped onto the VSCode `colors` keys
  that genuinely mean the same thing — the editor surface, gutter, selection and line highlight;
  sidebar/activity bar/panel/status bar/title bar; list hover and selection rows; input and widget
  surfaces; buttons and badges; the terminal ANSI palette; diff and git decoration colors; error and
  warning foregrounds; scrollbar slider states; and the `foreground`/`descriptionForeground`/
  `disabledForeground` text levels. Syntax comes from the theme's own `tokenColors`: every highlight
  bucket searches for its real textmate scope (`entity.name.function` for `function`,
  `keyword.control` for `keyword`, and so on), with proper scope matching — a rule for
  `variable.parameter` colors parameters without also recoloring plain variables — and a bucket
  with no rule of its own inherits its parent bucket's resolved color.

VSCode color families with no counterpart in this app (peek view, notebooks, testing,
merge-conflict decorations, debug toolbar, charts, bracket-pair colorization) are deliberately not
mapped; those tokens keep their derived value, which is still a color in the imported theme's own
family. The result is an ordinary Jerry theme file — every value literal, every line editable
afterwards.
