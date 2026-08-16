# Architecture decisions

Why Jerry's target architecture ([`overview.md`](./overview.md), [`crates.md`](./crates.md)) looks
the way it does — the reasoning and rejected alternatives, not just the rule. `CLAUDE.md` and the
`architecture` skill state each rule in a line or two and point here for the argument behind it,
deliberately, so they stay short.

This is a **decisions log, not a running narrative** — the distinction that matters after this
project's own `BUILD-LOG.md` (592 KB, a growing chronicle nobody could tell was current) got
deleted for exactly that reason (§4 below). A decision here is written once. If a later decision
changes an earlier one, it gets its **own new numbered entry** below, and the old entry's Status
line is updated to point at it — never edited back to "current." Add a new entry only for a real
decision (a new crate boundary, a new cross-cutting rule, a reversal) — not for every routine
application of one that already exists here.

## 1. Core crates stay free of `gpui`

**Status:** Accepted.

**Context:** `wt-core`, `pty-core`, and `lsp-core` were already built with no `gpui` dependency —
verified: the only two occurrences of the string `gpui` in those three crates are comments, one
about a version pin, the other noting that a real GPUI fake-clock test isn't possible without one.
This wasn't written down anywhere, so it was one dependency addition away from silently breaking.

**Decision:** No crate other than `crates/app` (and the planned `crates/jerry-cli`, which must
never gain one either) may depend on `gpui` or `gpui_platform`. This is the foundation the rest of
the target architecture is built on: it's what makes a headless CLI over the same domain logic
possible at all.

**Consequences:** A PR adding `gpui` to `wt-core`/`pty-core`/`lsp-core`'s `Cargo.toml` is a hard
reject, not a design discussion. Any type crossing from a core crate into `crates/app` and back
must be plain data — never a `gpui::Context`, `Window`, or similar (`crates/app/src/work_surface/agents.rs`
violates this today by taking `Context<AdeApp>` directly in agent-lifecycle methods; tracked as
follow-up, not retroactively blessed). `lsp-core`'s one cross-crate dependency (`pty-core`, for
`resolve_on_path`) staying a path dependency between two gpui-free crates is fine and doesn't need
repeating elsewhere.

## 2. Commands and queries, not loose functions, as the application-layer unit

**Status:** Accepted.

**Context:** `wt-core` exposes its capabilities as loose, well-named functions —
`commit_all_changes`, `attempt_merge`, `discard_worktree`, `resolve_hunk`, and about forty more.
Clean, but no shared shape: each has its own argument list and result type, so nothing can dispatch
them generically. That matters because the same action needs to be triggerable from the GPUI view
*and* from a future `crates/jerry-cli`, and actions need to compose (a merge that discards on
conflict, a commit that also pushes) without one caller knowing the other's calling convention.

Two shapes were considered. **Plain hexagonal — application services**: group the existing
functions into service structs behind port traits. Standard, but a service method is still just a
function with extra ceremony — it doesn't give the CLI and the view a common thing to dispatch, or
composition a unit to compose. **Command + Query, reifying every action as a value**: each
mutation becomes a `Command` with a typed input struct and typed outcome; each read becomes a
`Query`. One dispatch function serves any caller that can construct the input.

**Decision:** Adopt Command + Query.

```rust
pub trait Command {
    type Outcome;
    fn validate(&self, ctx: &Ctx) -> Result<(), ValidationError>;
    fn execute(self, ctx: &Ctx) -> Result<Self::Outcome, Error>;
}
```

`wt-core`'s existing functions map onto this almost one-to-one — `commit_all_changes` becomes
`CommitAllChanges { paths: Vec<PathBuf> } -> CommitAllChangesOutcome`, and so on. The
transformation is mechanical: the logic inside each function doesn't change, only its calling
convention does.

**Consequences:** `crates/jerry-cli` becomes possible without duplicating logic — it constructs the
same `Command` values the view does and calls the same `execute`. New capabilities are added as new
`Command`/`Query` types starting now, even though the existing `wt-core` functions aren't
retrofitted in this pass. This is deliberately *not* a command bus with an execution log — undo/redo
and provenance tracking already exist as their own hand-built mechanisms (`wt-core::undo`,
`crates/app/src/provenance/`); layering a generic event-sourced bus on top would duplicate them for
no immediate benefit. A real need for a unified execution log would be its own new entry here, not
an assumption baked into this one.

## 3. The view dispatches commands and queries; it never calls an adapter directly

**Status:** Accepted. Partially enforced — see Consequences.

**Context:** `crates/app`'s render layer currently calls straight into `wt-core` and, in one place,
straight into a raw process spawn: `graph_view/render.rs` alone has 109 `wt_core::` references,
`sidebar/render.rs` has 33, and `sidebar/render.rs:6534` shells out to
`std::process::Command::new("git")` directly, bypassing `wt-core` entirely. Several `render.rs`
files also call `cx.background_spawn`/`cx.spawn` directly around adapter calls, duplicating the
offload-to-background decision ad hoc at every call site.

This works today because `crates/app` is the only consumer of `wt-core`. It becomes a real problem
the moment a second consumer (`crates/jerry-cli`) exists: behavior implemented as "whatever the
render function happens to do around the `wt_core::` call" isn't available to the CLI, and a bug
fixed in the view's copy has to be separately remembered in the CLI's.

**Decision:** Render code (`render.rs`, anything returning `impl IntoElement` or implementing
`Render`) may only read state already held on `AdeApp` and dispatch a `Command`/`Query`, rendering
the outcome. It may never call `wt_core::`, `pty_core::`, `lsp_core::`, or `std::process::Command`
directly. The background-spawn decision moves into the dispatch layer, once, instead of being
repeated at every call site.

**Consequences:** This is currently violated at scale (204 direct adapter references across
`render.rs` files, per `.claude/conventions-baseline.json`) and is **not** retroactively fixed by
this decision — it's the target; the gap is tracked as GitHub issues. New render code must not add
new adapter calls, effective immediately, and this is now **mechanically checked**, not just
reviewed against: `.claude/hooks/check-conventions.sh` greps every `render.rs` file for
`wt_core::`/`pty_core::`/`lsp_core::`/`process::Command::new` and fails — in the pre-commit hook and
in CI — if the count exceeds the checked-in baseline. It's a textual ratchet (the count may only go
down), not a type-aware lint, and it needs no prerequisite. A full `clippy::disallowed-methods`
version, scoped to `render.rs` files, is still blocked on cleaning up `use super::*` globs first:
today a glob means a lint can't reliably tell which module a symbol resolved from — that cleanup is
tracked as its own issue, and the ratchet is the interim mechanical backstop until it lands.

## 4. `BUILD-LOG.md`/`ASSESSMENT.md` retired in favor of this decisions log

**Status:** Accepted.

**Context:** `BUILD-LOG.md` (592 KB, 7,715 lines) was a hand-written, append-only narrative
changelog of early build sessions. `ASSESSMENT.md` (24.7 KB) was a one-time, end-of-build
retrospective. Both were treated as living documentation — `CONTRIBUTING.md` told every new
contributor to read both before starting and mandated updating `BUILD-LOG.md` "alongside real
functional changes." Neither was current: `BUILD-LOG.md`'s last commit predated 241 of the
repository's 587 commits (~41% of project history undocumented in the file CONTRIBUTING called the
design record), and `ASSESSMENT.md` had exactly one commit — its own creation — and said so about
itself, opening by calling itself a stale snapshot left as-is rather than rewritten. Both files'
style — long narrative prose, revision numbers (R1–R12, R8.5a), design-history justification inline
with the artifact it documents — is also the pattern `CLAUDE.md`'s comment rule now excludes from
source comments; keeping the files around as the "proper place" for that material would have
undermined that rule immediately.

**Decision:** Delete both files. Git history retains every word for anyone who wants the
archaeology. Their replacement is this decisions log: entries written once, not maintained as a
running log, explicitly not a substitute for `git log`.

**Consequences:** `CONTRIBUTING.md`'s instructions to read and update `BUILD-LOG.md` are removed.
`README.md`'s `## Status` states current status directly rather than deferring to `ASSESSMENT.md`.
Design decisions worth recording going forward get a new numbered entry above, not an appended
paragraph in a long-running file.

## 5. `crates/test-support` is a real crate, not a feature-gated one

**Status:** Accepted.

**Context:** Test setup had no shared home, so it was copy-pasted instead: `fn git(dir, args)`
appeared ~30 times across `wt-core` and `app`, alongside 1,223 separate tempdir setups and 303
wall-clock waits. The obvious single crate to fix that has a trap in it — `crates/app`'s fixtures
need `gpui` (a test window, `VisualTestContext`), and `wt-core`/`pty-core`/`lsp-core` must be able
to dev-depend on the same crate without acquiring `gpui` (§1).

A Cargo feature (`test-support = { features = ["gpui"] }` for `app` only) looks like it solves
this and does not: features unify across a workspace build, so one crate enabling `gpui` enables it
for every other crate resolving the same dependency. The core crates' dev graph would silently
regain `gpui` — exactly the outcome §1 exists to prevent, and one no `Cargo.toml` review would
catch.

**Decision:** Two homes, split by dependency rather than by feature. `crates/test-support` is
`gpui`-free and depends only on `tempfile`; anything needing `gpui` lives in
`crates/app/src/test_support.rs`, inside the one crate already allowed to have it.

**Consequences:** `cargo tree -e normal,dev,build -i gpui` reaching only `crates/app` is a
checkable invariant, not a convention. A helper that "just needs a `TestAppContext`" is not added
to `crates/test-support` under any flag — it goes in `crates/app` or it is restructured to take
plain data. The policy those fixtures serve is [`docs/testing.md`](../testing.md).
