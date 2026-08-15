# Contributing

Thanks for looking at this project. It's a working prototype, and the process below exists to keep
it that way as more people touch it.

## Before you start

`gpui`/`gpui_platform` are plain git dependencies pinned to a specific `zed-industries/zed` commit
— see the [README](README.md#gpui-version-pin) for the pinned revision. Cargo fetches them
automatically; no manual checkout is needed.

Coding standards (what "no fake functionality" means, the exact hard rules on `unwrap`/`unsafe`/
paths/git argv, comment style, GPUI patterns) live in [`CLAUDE.md`](CLAUDE.md) — read that, not this
file, for how the code itself should look. This file covers the human contribution process around
it.

## What "done" means here

Every change must pass, locally, before you open a PR — the same checks CI runs on every push:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Run them together as `/check` if you're working through Claude Code. None are optional or "mostly
passing."

See [`docs/development-workflow.md`](docs/development-workflow.md) for how a change actually moves
from a GitHub issue to a merged PR in this repo, including which Claude Code skill covers which
step if you're using it.

## Verifying GPUI / `alacritty_terminal` / `gix` API usage

Never guess a GPUI, `alacritty_terminal`, or `gix` API signature. Before writing a call to one:

1. Check the fetched `gpui` git dependency's own `crates/gpui/examples/` first for real, runnable
   usage — Cargo checks it out under `~/.cargo/git/checkouts/zed-*/<rev>/crates/gpui/examples/`
   (find the exact path with `find ~/.cargo/git/checkouts -maxdepth 1 -iname 'zed-*'`).
2. Grep the rest of that same checkout for the call if the examples don't cover it.
3. For crates that checkout doesn't use itself (`gix` is the main example — Zed wraps the `git`
   CLI directly instead), read the actual fetched crate source under `~/.cargo/registry/src/`
   rather than trusting memory or documentation summaries.

If you genuinely cannot verify a signature this way, leave a `todo!("unverified: ...")` describing
exactly what's unverified rather than shipping a guess.

## `design_handoff_jerry_ade/`

This directory is a **design reference**, not application code: a high-fidelity HTML mockup
(`Jerry.dc.html`) plus a `tokens.rs` file with the exact colors/spacing/type this UI ("Jerry") was
built from. If you're working on UI:

- Read `design_handoff_jerry_ade/README.md` for the layout spec (exact zone heights, colors,
  states) before changing `crates/app/src/theme.rs` or any view module — the HTML mockup is the
  authoritative source for exact values when in doubt.
- Do not port markup out of `Jerry.dc.html` directly, and do not treat it as something to keep in
  sync going forward — it's a one-time handoff artifact, not a living spec.
- `design_handoff_jerry_ade/revision/` holds a later revision of the same handoff (see its own
  `CHANGELOG.md`) — prefer it over the top-level files where the two disagree.

## Architecture decisions

Significant design decisions are recorded as ADRs under `docs/adr/`, one file per decision — see
[`docs/adr/README.md`](docs/adr/README.md) for the index and
[`docs/architecture/overview.md`](docs/architecture/overview.md) for the current target
architecture and why. If your change makes an architectural call worth remembering, start from
[`docs/adr/template.md`](docs/adr/template.md) rather than explaining the decision in a code
comment or a commit message alone.

## Commit / PR expectations

- Keep commits focused; this project's own commit history (`git log`) is a reasonable model for
  scope and message style (`feat(app): ...`, `fix(pty-core): ...`, `docs: ...`).
- Branch names follow `<type>/<issue>-<slug>` (e.g. `fix/336-text-input-selection`).
- Don't add a dependency that duplicates something already available — check upstream Zed's own
  dependency choices first, as several crates in this workspace deliberately mirror its pinned
  versions (see e.g. `crates/app/Cargo.toml`'s `tree-sitter`/`alacritty_terminal` comments for why).
