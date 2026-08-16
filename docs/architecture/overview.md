# Architecture overview

Jerry is a desktop app: a GPUI view driving a git/process/LSP domain that must also work headless,
from a CLI, without dragging a UI framework along. This document is the target shape. It is not yet
the current shape — see [`decisions.md`](./decisions.md) §3 for the gap and the tracking issues.

## The dependency rule

Dependencies point inward, toward the domain. An outer layer may depend on an inner one; an inner
layer must never depend on an outer one. Concretely: **no crate below `crates/app` may ever gain a
`gpui` dependency.**

```
        ui (crates/app)              cli (crates/jerry-cli)
                \                        /
                 \                      /
                  v                    v
              application  — Commands, Queries, Ports (traits)
                          |
                          v
                       domain  — pure types, zero I/O
                          ^
                          |
    adapters: git (wt-core) · pty (pty-core) · lsp (lsp-core) · hooks
              implement the Ports; depend inward only
```

This already holds at the crate boundary: `wt-core`, `pty-core`, and `lsp-core` have zero `gpui`
dependency today. The violations are all inside `crates/app` itself, where render code calls
adapters directly instead of going through a command.

## Commands and queries, not services

The unit of application logic is a **Command** (a mutation) or a **Query** (a read), not a service
method. See [`decisions.md`](./decisions.md) §2 for the rationale;
the short version: reifying an action as a typed input + typed outcome is what lets the same
dispatch serve both the GPUI view and a `clap` CLI, and lets actions compose without either caller
knowing the other exists.

```rust
pub trait Command {
    type Outcome;
    fn validate(&self, ctx: &Ctx) -> Result<(), ValidationError>;
    fn execute(self, ctx: &Ctx) -> Result<Self::Outcome, Error>;
}
```

- **Command** — a mutation. `CommitAllChanges`, `AttemptMerge`, `DiscardWorktree`, `ResolveHunk`.
- **Query** — a read, no side effects. `DiffAgainstBase`, `BuildGraph`, `AheadBehind`.

`wt-core` already exposes almost exactly this set as loose functions (`commit_all_changes`,
`attempt_merge`, `discard_worktree`, `resolve_hunk`, …). Turning each into a `Command`/`Query` type
is a mechanical, low-risk transformation — the logic doesn't move, only its shape does.

## Ports and adapters

A **port** is a trait the application layer depends on; an **adapter** is the concrete
implementation. Adapters are where I/O lives:

| Port | Adapter today |
|---|---|
| Git operations | `wt-core` (via `gix` for reads, the real `git` CLI via explicit argv for writes) |
| Process spawning / PTY | `pty-core` (`portable-pty`) |
| Language server protocol | `lsp-core` |
| Agent hook side-channel | `crates/app/src/hooks/` (candidate extraction — see below) |

All three existing core crates are already synchronous, `Result`-returning, and blocking by design
— every public function in `wt-core` is documented as blocking, on the assumption that a GPUI caller
offloads it (`cx.background_spawn`). That contract does not change.

## Crate map (target)

See [`crates.md`](./crates.md) for the current Scope / Owns / Does not own table. The target adds
one crate:

- **`crates/jerry-cli`** — a `clap` binary that constructs the same `Command`/`Query` types the view
  uses and dispatches them. No gpui dependency. Exists to prove the application layer is genuinely
  UI-agnostic, and to make Jerry scriptable.

## What must move before the CLI is possible

`crates/app` currently holds ~24k lines of code with no `gpui` dependency at all, trapped inside the
UI crate because nothing forced it out. The largest and cleanest candidate is
`crates/app/src/hooks/` (~5,400 LOC — `server.rs`, `settings_file.rs`, `event.rs`, `store.rs`): a
loopback HTTP listener for the Claude-hook side-channel that is already, in effect, a headless
service. It is the pilot extraction (tracking issue: see `docs/architecture/crates.md`'s backlog
note).

Two mechanical blockers have to clear first, in this order:

1. **Replace `use super::*` globs with explicit imports** (284 occurrences in `crates/app`). Today a
   nominally pure `state.rs` can silently import `gpui` through its parent's glob, so no lint can
   tell a layering violation from a clean file. Explicit imports make the violation visible — and
   then enforceable with `clippy`'s `disallowed-types`.
2. **Remove adapter calls from `render.rs` files** — `graph_view/render.rs` alone has 109
   `wt_core::` references, and `sidebar/render.rs:6534` shells out to
   `std::process::Command::new("git")` directly. Render code should only ever dispatch a Command or
   Query and draw the result.

Both are tracked as GitHub issues rather than done in this pass — see the repo's issue tracker,
label `area:core`.
