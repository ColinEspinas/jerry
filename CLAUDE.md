# CLAUDE.md

Standards for working on Jerry, whether you're a human or an agent. `CONTRIBUTING.md` covers the
human-facing contribution process; this file is what both ultimately point at for how the code
itself should look.

## What Jerry is

A GPUI desktop app that supervises several AI coding agents at once, each in its own real git
worktree — an editor, a diff/review surface, and a terminal pane per session. See `README.md` for
the product description; this file only covers how to build it.

## Commands

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

All four together are the pre-commit gate — run them as `/check` before considering anything done.
`.claude/hooks/pre-commit-check.sh` runs the fast half (fmt + clippy) automatically before any
`git commit`, as a safety net; it deliberately skips the full test suite, which takes minutes, so
`/check` — not the hook alone — is what "done" means. None of the four are optional; a PR that
needs `#[allow(...)]` to silence a lint either fixes the underlying issue or justifies the allow
with a one-line comment.

Run the app with `cargo run --release -p app [repo-path]`. Use `--release` unless you're actively
recompiling every few seconds: a debug-profile GPUI build is commonly 5–20× slower for the per-frame
work this app does (layout/paint, `tree-sitter` parsing, terminal-grid decode), and no performance
observation made against a debug build is trustworthy.

## Architecture

Dependencies point inward. `wt-core`, `pty-core`, `lsp-core` are pure domain/infrastructure crates
with **zero `gpui` dependency** — that must never change. `crates/app` is the only crate allowed to
depend on `gpui`. Full detail: [`docs/architecture/overview.md`](docs/architecture/overview.md),
[`docs/architecture/crates.md`](docs/architecture/crates.md), and the ADRs under `docs/adr/`.

The application-layer unit is a **Command** (mutation) or **Query** (read), each a typed input
struct with a typed outcome — not a loose function, not a service method with an ad hoc signature.
This is what lets the same action be dispatched from the GPUI view and, eventually, from
`crates/jerry-cli`. See [`0002-command-query-core.md`](docs/adr/0002-command-query-core.md).

Render code (`render.rs`, anything implementing `Render`/`IntoElement`) dispatches a Command or
Query and draws the outcome. It never calls `wt_core::`, `pty_core::`, `lsp_core::`, or
`std::process::Command` directly. This is a *target*, not yet the current state — see
[`0003-ui-must-not-call-adapters.md`](docs/adr/0003-ui-must-not-call-adapters.md) for the gap and
the tracking issues. New code follows the rule starting now; existing violations are backlog, not
license to add more.

**No new `use super::*`.** Explicit imports only. Glob imports are why the layering violation above
can't be caught by a lint today — a "pure" module can silently receive `gpui` symbols through its
parent's glob. Existing globs aren't retrofitted in a given change unless that change already
touches the file for another reason.

## Rust standards

- **No `unwrap()`/`expect()` outside `#[cfg(test)]` code.** Every fallible operation in non-test
  code returns a `Result`. Test modules use `.expect("...")` freely, with a message describing what's
  being asserted.
- **`unsafe` only for justified FFI**, each site with its own `#[allow(unsafe_code)]` and a `SAFETY`
  comment explaining why it's sound: `main.rs`'s one `env::set_var` call, and the libc/Win32 process
  liveness/sampling calls in `hooks/settings_file.rs` and `status_bar/process_stats/{macos,windows}.rs`
  (there's no safe way to ask the OS for another process's CPU/memory or existence). `wt-core`,
  `pty-core`, and `lsp-core` have none, and shouldn't gain any. Any new `unsafe` needs the same
  treatment — a safe alternative checked first, and if there truly isn't one, flagged for discussion
  rather than added quietly.
- **`PathBuf`/`&Path`, never `String`, for filesystem paths.** Not stringly-typed and re-parsed at
  call sites.
- **Every git invocation is a real argument vector, never an interpolated shell string.** `wt-core`
  uses `gix` directly for reads and `std::process::Command` with explicit `&[&str]`/`&[OsString]`
  argv for the real `git` CLI. This matters for correctness (filenames with spaces/quotes), not just
  safety.
- **Every user-visible count goes through `crates/app/src/root/plural.rs`.** `plural::count(n,
  "file", None)` / `plural::form(n, "needs", "need")` — never `if n == 1 { "" } else { "s" }` at a
  call site, and never a hardcoded plural noun. Zero is plural in English and the helper knows that.
- **No fake functionality.** No UI element bound to hardcoded/sample data standing in for a real
  source, no simulated command output, no control that looks wired up but isn't. If something
  genuinely isn't built yet, say so explicitly — a visible "not implemented" state, or
  `todo!("unverified: ...")` with a real explanation — rather than fake a plausible result.
- **Don't guess a GPUI, `alacritty_terminal`, or `gix` API signature.** Check the fetched `gpui` git
  dependency's own `crates/gpui/examples/` first (`find ~/.cargo/git/checkouts -maxdepth 1 -iname
  'zed-*'` locates the checkout), then grep the rest of that checkout, then read the actual fetched
  crate source under `~/.cargo/registry/src/` for anything the checkout doesn't itself use (`gix` is
  the main case — Zed wraps the `git` CLI directly). If you still can't verify it, leave
  `todo!("unverified: ...")` rather than ship a guess.

## GPUI patterns

- Every `wt-core`/`lsp-core` call is documented blocking — offload it with `cx.background_spawn` or
  `cx.spawn`; never call one directly on the UI thread.
- Respect entity lifecycle: a `Task` spawned against an entity should be cancelled, not orphaned,
  when that entity drops. Don't hold a `Context<T>` past the callback that received it.
- Call `cx.notify()` exactly when state the render function reads has actually changed — not on
  every mutation reflexively, not skipped when it should fire.

## Comments

- `//!` module header: ≤10 lines. What this module's scope is, what it owns, what it explicitly does
  not own.
- `///` on public API: one line by default. Add parameters/errors/panics only when they're
  non-obvious from the signature.
- `//` inline: only a non-obvious *why*. Never restate what the next line already says.
- **Not source comments:** design history, issue archaeology, review-thread transcripts,
  alternatives-considered, revision IDs. That material belongs in a commit body or a
  `docs/adr/000N-*.md` entry, not embedded in the code it explains.

This is a real course-correction: comments are currently 25.7% of this codebase's lines, and some
files (`root/mod.rs` at 53%, `lib.rs` at 50%) are more prose than code. New code follows this rule.
Existing files aren't swept for it — but if you're already touching a file for something else, cut
what this rule would have flagged.

## Testing

`#[gpui::test]` + `TestAppContext`/`VisualTestContext` for anything touching a `Render`/`Entity` —
not a snapshot of stdout, which doesn't cover a retained-mode GPU UI. Name test modules by concern
(`mod change_row_selection_tests`, not `mod tests`) — the existing 175-name convention is good,
keep it. Fixtures live under a sibling `testdata/` directory, not inlined as string literals.

## Workflow

See [`docs/development-workflow.md`](docs/development-workflow.md) for how a change moves from a
GitHub issue to a merged PR, and which skill (`plan`, `architecture`, `implement`,
`rust-standards`, `verify`, `ship`, `review`, `triage`) covers which step.

- Branches: `<type>/<issue>-<slug>` (e.g. `fix/336-text-input-selection`). One convention, matching
  the conventional-commit style already used for commit messages (`feat(app): ...`,
  `fix(pty-core): ...`).
- Everything issue/PR-related goes through `gh`, not the web UI, so it's scriptable from a session:
  `gh issue view`, `gh pr create`, `gh pr review`.
- Don't add a dependency that duplicates something already available — check upstream
  `zed-industries/zed`'s own pinned versions first (see `crates/app/Cargo.toml`'s `tree-sitter`/
  `alacritty_terminal` comments for the pattern this project already follows).
