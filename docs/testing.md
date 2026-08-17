# Testing policy

What deserves a test in Jerry, what it may cost, and what gets deleted. `CLAUDE.md`'s "Testing"
section states the rule in two sentences and points here for the detail.

This exists because the suite grew to 3,564 tests and 109,987 lines inside `#[cfg(test)]` blocks —
**45.3% of the workspace's 242,846 lines** — with `fn git(dir, args)` copy-pasted ~30 times and
1,223 separate tempdir setups. Nothing in the repo said what a test was *for*, so nothing said when
not to write one.

## The three tiers

Every test belongs to exactly one tier, and the tier decides what it may do and what it may cost.

| Tier | Marker | May do | Must not | Budget |
|---|---|---|---|---|
| `unit` | plain `#[test]` | pure logic, tempdir filesystem, a short-lived process the toolchain already guarantees (`git`, `/bin/sh`) | open a gpui window, spawn a language server or an agent, sleep, put an upper bound on elapsed time | < 10 ms, or a few hundred once it shells out |
| `ui` | `#[gpui::test]` + `TestAppContext` / `VisualTestContext` | real git fixtures, real key/action dispatch | external language servers, real agent processes, sleep | < 2 s |
| `external` | `#[ignore = "external: <binary>; see docs/testing.md"]` | real `rust-analyzer` / `typescript-language-server` / `pyright` / `gopls`, real OS process-tree semantics | — | dedicated CI job only |

Tiers 1 and 2 are the PR gate. Tier 3 never is.

Today that is 2,186 plain `#[test]` and 1,006 `#[gpui::test]` in `crates/app` alone — the split
already roughly exists, it has just never been named or enforced.

A `unit` test that shells out to `git` against a tempdir repository is still a `unit` test, and so
is one that spawns `/bin/sh` to stand in for a real login shell: the budget is what separates the
tiers in practice, and a seeded fixture repo costs a few hundred milliseconds of `git` at most.
What pushes a test into `ui` is needing a window; what pushes it into `external` is needing a
binary neither the OS nor this repo's own toolchain already guarantees — `rust-analyzer`,
`pyright` — or a real agent process.

## What deserves a test

- One test per behaviour a **user** could notice — not one per branch of the implementation.
- One regression test per fixed bug, named after the symptom.
- Pure derivations (colours, geometry, formatting) get **one table-driven invariant test**, not one
  test per input.
- Explicitly not worth a test: a constant equalling its own literal; a field round-tripping through
  its own getter; "a render function returned some element"; exhaustively enumerating a sweep the
  code itself already iterates over.

## Deletion criteria

The five reasons a test gets removed. A removal cites exactly one of them, by number, in the PR
body:

1. **Sweep redundancy** — N tests differing only by an input value. Collapse to one table-driven test.
2. **Implementation mirroring** — the assertion recomputes what the code under test does.
3. **Vacuous** — would still pass with the feature removed, or only asserts on test-local data.
4. **Pixel over-specification** — asserts an exact `Pixels` literal with no invariant behind it.
   Keep `all_three_elbow_pieces_paint_their_horizontal_stroke_on_the_very_same_pixel_row`;
   drop the "...equals 12.5px" siblings.
5. **Duplicate coverage across layers** — the same behaviour asserted in both a `unit` and a `ui`
   test. Keep the cheaper one, unless the wiring itself is what is at risk.

## Hard rules

- **No `thread::sleep` in test code.** Use `cx.run_until_parked()` or
  `test_support::wait_until(deadline, cond)`. There are **303** existing wall-clock waits, including
  ten `from_millis(500)`; they are the purge issue's problem, but the rule starts here. The
  `from_secs(9)` this list used to name is gone: `hooks::server`'s
  `many_slow_clients_at_once_cannot_starve_a_real_hook` now waits on the drip-feeders' own
  cut-off signal instead of guessing a duration.
- **No upper bound on elapsed time as an assertion.** `assert!(elapsed < limit)` fails whenever CI
  is busy, and any limit loose enough to survive that no longer distinguishes the behaviour it was
  written for — this rule is a real Linux CI failure, not a hypothetical (GitHub PR #451). Assert
  the observable effect instead: have the subject's command touch a marker file on completion, and
  assert the marker's absence, optionally over a window with `test_support::stays_false`. A *lower*
  bound (`elapsed >= its own timeout`) is fine and often necessary — no slowness can break one, and
  it is what stops such a test from passing vacuously when the subject returns early for an
  unrelated reason. Genuine hangs are the runner's job: `.config/nextest.toml`'s `slow-timeout`
  kills the test, not the run.
- Any test that spawns a process owns its teardown, via an RAII guard from `test-support`. A
  grandchild the test never spawned itself — a shell's own backgrounded job — is outside what a
  guard can reach; keep it short-lived rather than pretending otherwise.
- Any test that opens a file watcher shuts it down. A leaked watcher thread fails the tier's budget.
- Every `#[ignore]` carries a reason string naming exactly what is missing.
- Existing conventions that stay, unchanged: fixtures live under a sibling `testdata/` directory,
  never inlined as string literals; test modules are named by concern
  (`mod change_row_selection_tests`, not `mod tests`).

## `crates/test-support`

The shared fixtures, dev-dependency only. It is **`gpui`-free** so `wt-core`, `pty-core` and
`lsp-core` can dev-depend on it without violating
[`decisions.md` §1](architecture/decisions.md) — including behind a Cargo feature, which would not
help: workspace feature unification would pull `gpui` into their dev graph anyway. GPUI-flavoured
helpers live in `crates/app/src/test_support.rs` instead.

Add it with `test-support = { path = "../test-support" }` under `[dev-dependencies]`, then
`use test_support::{git, seed_repo};`.

### Running git

| Helper | Does |
|---|---|
| `git(dir, &["commit", "-m", "x"])` | runs git, panics with both streams on failure |
| `git_output(dir, args) -> String` | trimmed stdout of a successful invocation |
| `git_try(dir, args) -> Output` | runs git without asserting — for tests whose subject *is* a failure |
| `git_with_env(dir, args, &[("GIT_AUTHOR_DATE", …)])` | as `git`, with extra environment variables |
| `write_file(dir, "src/a.rs", "…")` | writes a `dir`-relative file, creating parent directories |
| `commit(dir, "a.txt", "1\n", "message")` | write + `add` + `commit` |
| `commit_at(dir, "a.txt", "1\n", "message", 1_700_000_000)` | as `commit`, with an explicit author/committer timestamp |

Argv, never an interpolated shell string — the same rule production code follows (`CLAUDE.md`).

### Seeding repositories

| Helper | Yields |
|---|---|
| `seed_repo() -> TempDir` | branch `main`, one commit (`file.txt`), clean tree |
| `seed_repo_at(dir)` | the same, into an existing directory |
| `seed_empty_repo() -> TempDir` / `seed_empty_repo_at(dir)` | branch `main`, identity configured, **no** commits |
| `seed_three_commits(dir)` | `first`/`second`/`third`, each rewriting `a.txt`, clean tree |
| `seed_commits(dir, count)` | `count` commits, cheap (`--allow-empty` after the first) |
| `seed_bare_remote() -> TempDir` | a bare repo on `main`, as a push/fetch target |
| `add_worktree(repo, "feature", &path)` | a real second working copy on a new branch |

Every seeded repo configures `user.email`, `user.name`, `commit.gpgsign=false` and
`core.autocrlf=false`, so a fixture behaves the same on a machine with commit signing configured
globally, and on Windows — where git otherwise ships `core.autocrlf=true` and would put `\r` into
every content, diff and blame assertion.

### Waiting and teardown

| Helper | Does |
|---|---|
| `wait_until(deadline: Duration, cond) -> bool` | polls until `cond` holds or the deadline passes; the caller writes the assertion message |
| `wait_until_every(interval, deadline, cond) -> bool` | as `wait_until`, with an explicit poll interval — for a check that perturbs what it measures |
| `stays_false(window: Duration, cond) -> bool` | the inverse — proves something stays quiet for a bounded window |
| `ChildGuard` | kill-on-drop wrapper for a spawned `Child`; derefs to `Child`, plus `spawn`, `is_running`, `kill_and_wait`, `into_inner` |

`wait_until` is the *only* sanctioned wall-clock wait in the workspace. Anything a GPUI executor
drives waits with `cx.run_until_parked()` instead.

Reach for `wait_until_every` only when the check itself is not free — a probe that takes a
connection slot in the server it is asking about, a check that contends for the lock the subject
needs. At `wait_until`'s 10 ms such a check measures its own polling rate. Prefer, where it exists,
a signal the subject already emits: perturbing nothing beats polling coarsely.

### GPUI fixtures (`crates/app/src/test_support.rs`)

- `open_test_app(cx, repo_path) -> (Entity<AdeApp>, &mut VisualTestContext)` — a real test window
  with in-memory settings, so a test never reads or writes the developer's `settings.toml`.
- `open_test_app_with_settings(cx, repo_path, settings, settings_path)` — for tests that assert on
  what gets persisted.

`crate::root::focus::palette_focus_tests::open_test_app` is a re-export of the first, kept so the
~500 call sites that still name it keep resolving while they are migrated (GitHub issue #425).

## Running tests

`cargo nextest run --workspace` is part of the pre-commit gate (`CLAUDE.md`, `/check`) and runs on
every PR in CI, on Linux and macOS. It covers the `unit` and `ui` tiers: the `external` tier is
`#[ignore]`d, and nextest skips ignored tests unless asked for them.

**Windows is not a test platform yet.** Its CI job builds the workspace and runs only
`status_bar::process_stats`, the FFI backend that exists nowhere else. The full suite does not pass
there — a single trial run scored 134 failures and 26 timeouts, mostly paths formatted into strings
and forward slashes baked into expected values — and GitHub issue #440 tracks fixing it. Until then
nothing asks a contributor to run the suite on Windows, and a Windows result is not a gate.

While iterating, scope it to what you touched:

```sh
cargo nextest run -p wt-core
cargo nextest run -p test-support
cargo nextest run -p app --lib -E 'test(/my_concern_tests/)'
```

### The `external` tier

Never on a PR. It runs in `ci.yml`'s `External (real language servers)` job, on a nightly schedule
and on `workflow_dispatch`. To run it locally you need the servers on `PATH` and a live npm
registry (one fixture does its own `npm install typescript@5`):

```sh
rustup component add rust-analyzer
npm install -g "@vue/language-server@3.3.8" typescript-language-server pyright
cargo nextest run --workspace --profile external --run-ignored all
```

`--profile external` inherits the `ci` profile and raises `slow-timeout` to 120s — a real
handshake plus a first index pass legitimately takes seconds, which the default 30s/2min budget
cannot tell apart from a hang.

Only `@vue/language-server` is pinned, to the version `crate::language`'s own module docs were
written against. `typescript-language-server` and `pyright` are knowingly unpinned: no commit
message or comment in this repo records the version their tests were written against, and the only
established fact is negative — `typescript-language-server@5.3.0` fails them. The nightly job
prints `npm ls -g --depth=0` so a pin can eventually be derived from a run that actually passes,
rather than guessed. `gopls` is not installed: `crate::language` gives Go `lsp: None`, so nothing
in this workspace ever spawns it.
