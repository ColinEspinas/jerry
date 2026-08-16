# Settings

- **Code:** `crates/app/src/settings/`
- **Tokens:** `theme::{settings, toggle, button, zone::SETTINGS_*}`

## What it's for

Settings is **a view of a config file**, not a database of preferences with a file export. The file
(`~/.config/jerry/settings.toml`) is the source of truth; this surface reads and writes it, and says
so on every page.

That framing is deliberate and it shows up in the design: each page names the file, lists the exact
keys it owns, and prints the real TOML (or JSON) for its own current values at the foot of the page.
A user who prefers the editor loses nothing, and a user who prefers the UI learns the file's shape
while using it.

## Structure

A **separate surface, not a modal**: it replaces the three zones while the title bar and status bar
stay put. `esc` — rendered as a keycap in the nav header — returns to the workspace.

### Nav

`theme::zone::SETTINGS_NAV_WIDTH` wide, four groups:

| Group | Pages |
|---|---|
| Workspace | General · Agents · Worktrees |
| Interface | Appearance & scaling · Themes · Keybindings |
| Editor | Editor · Language servers |
| Other | Notifications · Integrations · About |

`settings::state::SettingsPage` is the eleven-variant enum; `SettingsPage::ALL` is the order and
`SettingsPage::label` the rendered names. Each page also has a stable `id()` written by hand rather
than derived from the variant name, so renaming a variant cannot silently re-key its elements.

### Content column

Capped at `theme::zone::SETTINGS_CONTENT_MAX_WIDTH` — header block and scrollable body share the cap
— and left-aligned inside the surface padding. Long lines of prose in a settings page are the
fastest way to make it unreadable.

Every page is: header block (title, one-line rationale) · **config banner** · sections · **snippet
block**.

**Config banner** — the file path, the page's own key list, a `TOML | JSON` switch, and an `Open
file` button. `store::config_keys_line` is the key list, and it is deliberately narrower than the
mockup's fixture: it names only the `Settings` fields this app really persists, rather than
inheriting a list that included settings that were never implemented.

**Snippet block** — `In settings.toml`, then the page's real keys with their real current values,
serialised from the live `Settings` struct by `store::snippet_lines`. It is generated, not
transcribed, which is why it cannot drift from what the file actually contains.

`store::ConfigPage` is the five pages that have a banner and snippet (General, Appearance, Theme,
Editor, Notifications) — the ones backed by scalar settings. Agents, Worktrees, Keybindings and
Language servers are card/list pages over real discovered state, not over a config table.

### Row controls — four kinds, no fifth

One pattern for every scalar setting: label plus hint on the left, control on the right, bottom
border. The control is one of `toggle`, `stepper`, `choice` (a segmented control matching the code
surface's Diff/File toggle) or `path` (value plus a `Change…` button). They live in
`settings::widgets` and are shared by every page.

**Hints carry the reasoning, not a restatement of the label.** *"Past eight the rail stops being
glanceable"*, *"Costs a cold rebuild when the toolchain changes"*. That tone is part of the design:
a hint that repeats the label is worse than no hint, because it trains the reader to skip them.

### Card pages

Agents, Worktrees and Language servers share one card shape: a bordered card of rows on a slightly
lighter surface, separated by hairlines, with a footer carrying totals and the page's one action.
Rows are fixed-width columns so that the same field lines up down the card.

These pages show **real discovered state** — `settings::state` performs real `$PATH` detection for
agent binaries and language servers, so a "ready" dot means the binary was actually found.

### Themes

Theme cards over a swatch strip. Six built-ins live as real literal palette files
(`settings::builtin_themes`, `assets/themes/*.toml`), and a user's own themes load from
`~/.config/jerry/themes/*.toml` (`settings::custom_theme` — the format, its validation, and
import/export). `settings::vscode_theme` imports a VS Code theme.

The mechanism underneath is `theme.rs`'s `ColorToken`: every token has a stable key
(`"surface.window"`, `"syntax.keyword"`) and its own Jerry Dark literal default, a theme file names
tokens by those keys, and live resolution is a plain hash lookup. Jerry Dark is the identity case —
no palette installed, so every token resolves to its own compiled default unchanged. See
[`docs/themes.md`](../themes.md) for the file format from a user's side.

### Keybindings

Rows of command · context · keycaps · source (`base` or `user`). The list is resolved off the **live
registered** `gpui::KeyBinding`s through `keymap::resolve_keystroke`, not a hand-maintained table —
which is what stops it from drifting the way a hand-copied list once did (a wrong context label, a
stale order). Overrides persist through `keymap_overrides`.

## Rules that matter

- **The file is the source of truth.** A setting that the UI can change but the file cannot express
  is a bug. Every page says where its values live.
- **The snippet is generated from live state**, never written by hand.
- **Four row controls, and adding a fifth is a design decision.** The four cover every scalar shape
  Jerry has needed; a bespoke control on one page is the beginning of two idioms.
- **Hints explain, they don't restate.**
- **Detection is real.** A "ready" dot, a resolved binary path, a version string — all read off the
  real machine. A page that cannot detect something says so rather than showing a plausible default.
- **Page ids are hand-written**, decoupled from variant names.
- **Settings replaces the zones; it is not a modal.** The status bar stays visible, which is why
  long-running feedback is rendered there — see [`layout.md`](./layout.md#status-bar).

## Not built yet

- **Notifications and Integrations are thin.** They exist as pages with real nav entries; the
  surface area behind them is small compared to Agents/Worktrees/Themes.
- **Not every page has a config banner.** `store::ConfigPage` covers five; the card pages are over
  discovered state rather than a config table, and deliberately have neither banner nor snippet.
- **Window-controls style is a cosmetic preview only.** Overriding it changes the title bar and the
  keycap glyphs, not which physical key is bound — GPUI resolves that at compile time. See
  [`layout.md`](./layout.md#title-bar--two-variants).
