# Phosphor Icons (vendored)

The `.svg` files in this directory are **unmodified** Phosphor Icons, copied byte-for-byte from
the upstream project's own `assets/bold/` tree.

| | |
|---|---|
| Project | [Phosphor Icons](https://phosphoricons.com) |
| Source repository | <https://github.com/phosphor-icons/core> |
| Release | `v2.0.8` |
| Commit vendored from | `2b75f3ad12b420c9504ef05df8d2564a28f8500e` (`main`) |
| Upstream path | `assets/bold/<name>-bold.svg` |
| Licence | MIT — see [`LICENSE.txt`](LICENSE.txt), Phosphor's own unmodified licence text |
| Copyright | © 2023 Phosphor Icons |

## Weight

All twelve files are the **bold** weight. `design_handoff_jerry_ade/revision 5/`'s
`REVISION-2026-08-14.md` §8 states the rule verbatim:

> **Weight:** `bold` at 15–17px (`regular`'s 1.5px stroke reads thin against `#5e646a`);
> `regular` only at 20px+.

Every named size in `crates/app/src/icons.rs` is ≤ 17px, so bold is the correct — and only
needed — weight today. `icons::weight_for_size` and its tests hold that rule: adding a 20px+
size fails the build's test run until the matching `regular` files are vendored here too.

## Renaming

Upstream names each bold file `<name>-bold.svg`; the `-bold` suffix is dropped here because the
weight is a property of *this whole directory*, not of an individual file, and the app looks
icons up by the design handoff's own names (`tree-structure`, `caret-down`, ...). The file
*contents* are untouched.

## The twelve files, and the slot each one serves

Straight from `REVISION-2026-08-14.md` §8's mapping table, plus the one glyph §4u names outside
it (the overflow menu's Settings row):

| Slot | File |
|---|---|
| sidebar strip: worktrees | `tree-structure.svg` |
| sidebar strip: history | `clock-counter-clockwise.svg` |
| sidebar strip: problems | `warning.svg` |
| panel tabs: Files | `folder.svg` |
| panel tabs: Search | `magnifying-glass.svg` |
| panel tabs: Changes | `git-branch.svg` |
| count row: replace | `arrows-left-right.svg` |
| count row: filter | `funnel.svg` |
| count row: fold-all | `caret-down.svg` |
| rail footer: prune | `trash.svg` |
| tab strip: terminal | `terminal-window.svg` |
| overflow menu: Settings (§4u, issue #290) | `sliders-horizontal.svg` |

`crates/app/src/icons.rs` embeds these with `include_bytes!` and serves them through
`crate::fonts::Assets`, so the built binary carries them and does not read this directory at
runtime.
