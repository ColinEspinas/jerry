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
| `unit` | plain `#[test]` | pure logic, tempdir filesystem | spawn a process, open a gpui window, sleep, assert on wall-clock | < 10 ms |
| `ui` | `#[gpui::test]` + `TestAppContext` / `VisualTestContext` | real git fixtures, real key/action dispatch | external language servers, real agent processes, sleep | < 2 s |
| `external` | `#[ignore = "external: <binary>; see docs/testing.md"]` | real `rust-analyzer` / `typescript-language-server` / `pyright` / `gopls`, real OS process-tree semantics | — | dedicated CI job only |

Tiers 1 and 2 are the PR gate. Tier 3 never is.

Today that is 2,186 plain `#[test]` and 1,006 `#[gpui::test]` in `crates/app` alone — the split
already roughly exists, it has just never been named or enforced.

A `unit` test that shells out to `git` against a tempdir repository is still a `unit` test: the
budget is what separates the tiers in practice, and a seeded fixture repo costs a few hundred
milliseconds of `git` at most. What pushes a test into `ui` is needing a window; what pushes it
into `external` is needing a binary the repo does not vendor or a real agent process.

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
  a `from_secs(9)` and ten `from_millis(500)`; they are the purge issue's problem, but the rule
  starts here.
- Any test that spawns a process owns its teardown, via an RAII guard from `test-support`.
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

Every seeded repo configures `user.email`, `user.name` and `commit.gpgsign=false`, so a fixture
behaves the same on a machine with commit signing configured globally.

### Waiting and teardown

| Helper | Does |
|---|---|
| `wait_until(deadline: Duration, cond) -> bool` | polls until `cond` holds or the deadline passes; the caller writes the assertion message |
| `stays_false(window: Duration, cond) -> bool` | the inverse — proves something stays quiet for a bounded window |
| `ChildGuard` | kill-on-drop wrapper for a spawned `Child`; derefs to `Child`, plus `spawn`, `is_running`, `kill_and_wait`, `into_inner` |

`wait_until` is the *only* sanctioned wall-clock wait in the workspace. Anything a GPUI executor
drives waits with `cx.run_until_parked()` instead.

### GPUI fixtures (`crates/app/src/test_support.rs`)

- `open_test_app(cx, repo_path) -> (Entity<AdeApp>, &mut VisualTestContext)` — a real test window
  with in-memory settings, so a test never reads or writes the developer's `settings.toml`.
- `open_test_app_with_settings(cx, repo_path, settings, settings_path)` — for tests that assert on
  what gets persisted.

`crate::root::focus::palette_focus_tests::open_test_app` is a re-export of the first, kept so the
~500 call sites that still name it keep resolving while they are migrated (GitHub issue #425).

## Running tests

`cargo test --workspace` is deliberately **not** part of the pre-commit gate — see `CLAUDE.md` and
[GitHub issue #348](https://github.com/ColinEspinas/jerry/issues/348). Run what you touched:

```sh
cargo nextest run -p wt-core
cargo nextest run -p test-support
cargo nextest run -p app --lib -E 'test(/my_concern_tests/)'
```
