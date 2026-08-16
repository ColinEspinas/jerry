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
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Run them together as `/check` if you're working through Claude Code. None are optional or "mostly
passing." `cargo test --workspace` is intentionally not part of this list right now — see
[issue #348](https://github.com/ColinEspinas/jerry/issues/348); run tests relevant to what you're
touching manually instead.

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

## Design documentation

What each UI surface is for, how it's put together, and the invariants a change must not break
live in [`docs/design/`](docs/design/README.md) — one page per surface, plus
[`docs/design/decisions.md`](docs/design/decisions.md), a numbered log in the same shape as the
architecture one below. If you're working on UI, read
[`docs/design/principles.md`](docs/design/principles.md) and
[`docs/design/vocabulary.md`](docs/design/vocabulary.md) first, then the page for the surface
you're changing.

These pages deliberately never reprint a value. `crates/app/src/theme.rs` is the source of truth
for every colour and dimension — each token carries its own doc comment, and the bundled themes in
`assets/themes/` are generated from it — so a page names the token rather than a hex, and cannot
drift from it.

**A UI change updates the relevant page in the same PR.** If it makes a call a future contributor
would otherwise re-litigate, add a numbered entry to `docs/design/decisions.md` rather than
explaining it in a code comment.

## Architecture decisions

Significant design decisions and their reasoning live in
[`docs/architecture/decisions.md`](docs/architecture/decisions.md), one numbered entry per
decision — see [`docs/architecture/overview.md`](docs/architecture/overview.md) for the current
target architecture itself. If your change makes an architectural call worth remembering, add a new
entry there rather than explaining the decision in a code comment or a commit message alone.

## Commit / PR expectations

- Keep commits focused; this project's own commit history (`git log`) is a reasonable model for
  scope and message style (`feat(app): ...`, `fix(pty-core): ...`, `docs: ...`).
- Branch names follow `<type>/<issue>-<slug>` (e.g. `fix/336-text-input-selection`).
- Don't add a dependency that duplicates something already available — check upstream Zed's own
  dependency choices first, as several crates in this workspace deliberately mirror its pinned
  versions (see e.g. `crates/app/Cargo.toml`'s `tree-sitter`/`alacritty_terminal` comments for why).
