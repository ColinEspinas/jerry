# 0002: Commands and queries as the application-layer unit

## Status

Accepted.

## Context

`wt-core` exposes its capabilities as loose, well-named functions: `commit_all_changes`,
`attempt_merge`, `discard_worktree`, `resolve_hunk`, and about forty more across `diff.rs`,
`merge.rs`, `undo.rs`, `rebase.rs`. This is clean, but it has no shared shape — each function has its
own argument list and its own result type, so nothing can dispatch them generically. That matters
because the goal (see [`../architecture/overview.md`](../architecture/overview.md)) is for the same
action to be triggerable from the GPUI view and from a `clap` CLI, and for actions to compose (a
merge that discards on conflict, a commit that also pushes) without one caller having to know the
other's calling convention.

Two shapes were considered:

1. **Plain hexagonal — application services.** Group the existing functions into service structs
   behind port traits. Standard, well understood, but a service method is still just a function with
   extra ceremony: it doesn't give the CLI and the view a common thing to dispatch, and it doesn't
   give composition a unit to compose.
2. **Command + Query, reifying every action as a value.** Each mutation becomes a `Command` with a
   typed input struct and a typed outcome; each read becomes a `Query`. One dispatch function serves
   any caller that can construct the input.

## Decision

Adopt Command + Query. A mutation is a `Command`, a read is a `Query`:

```rust
pub trait Command {
    type Outcome;
    fn validate(&self, ctx: &Ctx) -> Result<(), ValidationError>;
    fn execute(self, ctx: &Ctx) -> Result<Self::Outcome, Error>;
}
```

`wt-core`'s existing functions map onto this almost one-to-one — `commit_all_changes` becomes
`CommitAllChanges { paths: Vec<PathBuf> } -> CommitAllChangesOutcome`, `attempt_merge` becomes
`AttemptMerge { .. } -> MergeOutcome`, and so on. The transformation is mechanical: the logic inside
each function doesn't change, only its calling convention does.

## Consequences

- `crates/jerry-cli` becomes possible without duplicating logic: it constructs the same `Command`
  values the view does and calls the same `execute`.
- New capabilities are added as new `Command`/`Query` types, not new ad-hoc functions — this is the
  rule new code follows starting now, even though the existing `wt-core` functions aren't
  retrofitted in this pass (tracked separately, see `docs/architecture/overview.md`).
- This is deliberately *not* a command bus with an execution log. Undo/redo and provenance tracking
  already exist as their own hand-built mechanisms (`wt-core::undo`, `crates/app/src/provenance/`);
  layering a generic event-sourced bus on top would duplicate them for no immediate benefit. If a
  real need for a unified execution log emerges, it's a new ADR, not an assumption baked into this
  one.
