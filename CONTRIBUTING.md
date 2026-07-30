# Contributing

Thanks for looking at this project. It's a working prototype (see
[ASSESSMENT.md](ASSESSMENT.md) for an honest read on what's solid versus rough), and the
rules below exist to keep it that way as more people touch it.

## Before you start

Read [BUILD-LOG.md](BUILD-LOG.md) for the step-by-step history of how each part of this
app was built and why (including the reasoning behind API/version choices), and
[ASSESSMENT.md](ASSESSMENT.md) for a candid assessment of what genuinely works end to end
versus what's rough or unverified. Both are unusually detailed on purpose — treat them as
the design record, not just changelog trivia, and try to keep new work consistent with the
decisions they document rather than silently reversing them.

`gpui`/`gpui_platform` are plain git dependencies pinned to a specific
zed-industries/zed commit — see the [README](README.md#gpui-version-pin) for the pinned
revision. Cargo fetches them automatically; no manual checkout is needed.

## What "done" means here

Every change must pass, locally, before you open a PR (CI in `.github/workflows/ci.yml`
runs the same checks on Linux, plus a build-only job on macOS/Windows):

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

None of these are optional or "mostly passing." A PR that needs `#[allow(...)]` to silence
a clippy lint should either fix the underlying issue or justify the allow with a comment
explaining why the lint doesn't apply — see existing code for the pattern (this codebase
comments its judgment calls, not just its code).

## Hard rules, established throughout the existing code

These aren't a style preference layered on top of the code — they're patterns the existing
crates already follow everywhere, and new code is expected to match them:

- **No `unwrap()`/`expect()` outside `#[cfg(test)]` code.** Every fallible operation in
  non-test code returns a `Result` (see `crates/wt-core/src/error.rs`'s `Error`/`GitExit`
  types, or `crates/pty-core`'s and `crates/lsp-core`'s own error types, for the pattern).
  Test modules use `.expect("...")` freely with a message describing what's being
  asserted — that's fine and expected there.
- **No `unsafe`.** Nothing in `wt-core`, `pty-core`, `lsp-core`, or `app` uses an `unsafe`
  block today. If you find yourself reaching for one, that's a sign to look for a safe
  API first (or, if there truly isn't one, to flag it explicitly for discussion rather
  than add it quietly).
- **`PathBuf`, not `String`, for filesystem paths.** Paths are typed as `PathBuf`/`&Path`
  throughout the public API of every crate (see `wt-core::Worktree::path`,
  `pty-core`'s spawn APIs, etc.) — not stringly-typed and re-parsed at call sites.
- **Every git invocation is a real argument vector, never an interpolated shell
  string.** `wt-core` uses `gix` directly for reads, and `std::process::Command` with
  explicit `&[&str]`/`&[OsString]` argv for anything that shells out to the real `git`
  CLI (worktree add/remove, merges) — see that crate's module docs. This matters for
  correctness (filenames with spaces/quotes) as much as safety; don't build git commands
  by formatting a string.
- **No fake functionality.** No UI element bound to hardcoded/sample data standing in for
  a real data source, no simulated command output, no button that looks wired up but
  isn't. If a subsystem genuinely isn't built yet, the convention in this codebase is to
  say so explicitly (a visible "not implemented" state, or a `todo!("unverified: ...")`
  with a real explanation) rather than fake a plausible-looking result. ASSESSMENT.md
  exists specifically to keep this honest at the project level — don't undermine it by
  landing something that only looks done.

## Verifying GPUI / `alacritty_terminal` / `gix` API usage

This project's own build history (see BUILD-LOG.md) was built around a specific
discipline: never guess a GPUI, `alacritty_terminal`, or `gix` API signature. Before
writing a call to one:

1. Check the fetched `gpui` git dependency's own `crates/gpui/examples/` first for real,
   runnable usage — Cargo checks it out under
   `~/.cargo/git/checkouts/zed-*/<rev>/crates/gpui/examples/` (find the exact path with
   `find ~/.cargo/git/checkouts -maxdepth 1 -iname 'zed-*'`).
2. Grep the rest of that same checkout for the call if the examples don't cover it.
3. For crates that checkout doesn't use itself (`gix` is the main example — Zed wraps the
   `git` CLI directly instead), read the actual fetched crate source under
   `~/.cargo/registry/src/` rather than trusting memory or documentation summaries.

If you genuinely cannot verify a signature this way, leave a `todo!("unverified: ...")`
describing exactly what's unverified rather than shipping a guess.

## `design_handoff_jerry_ade/`

This directory is a **design reference**, not application code: a high-fidelity HTML
mockup (`Jerry.dc.html`) plus a `tokens.rs` file with the exact colors/spacing/type this
UI ("Jerry") was built from. If you're working on UI:

- Read `design_handoff_jerry_ade/README.md` for the layout spec (exact zone heights,
  colors, states) before changing `crates/app/src/theme.rs` or any view module — the HTML
  mockup is the authoritative source for exact values when in doubt.
- Do not port markup out of `Jerry.dc.html` directly, and do not treat it as something to
  keep in sync going forward — it's a one-time handoff artifact, not a living spec.
- `design_handoff_jerry_ade/revision/` holds a later revision of the same handoff (see its
  own `CHANGELOG.md`) — prefer it over the top-level files where the two disagree.

## Commit / PR expectations

- Keep commits focused; this project's own commit history (see `git log`) is a reasonable
  model for scope and message style (`feat(app): ...`, `fix(pty-core): ...`,
  `docs: ...`).
- Update `BUILD-LOG.md` (and, if the change is significant enough to shift the overall
  picture, `ASSESSMENT.md`) alongside real functional changes — both are meant to stay a
  living, accurate record, not a one-time snapshot.
- Don't add a dependency that duplicates something already available (check upstream
  Zed's own dependency choices first, as several crates in this workspace deliberately
  mirror its pinned versions — see e.g. `crates/app/Cargo.toml`'s `tree-sitter`/
  `alacritty_terminal` comments for why).
