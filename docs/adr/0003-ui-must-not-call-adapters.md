# 0003: The view dispatches commands and queries; it never calls an adapter directly

## Status

Accepted. Partially enforced — see Consequences: a textual ratchet blocks the count of
violations from growing; a full, type-aware lint blocking every violation is still pending.

## Context

`crates/app`'s render layer currently calls straight into `wt-core` and, in one place, straight into
a raw process spawn:

- `graph_view/render.rs` has 109 `wt_core::` references.
- `sidebar/render.rs` has 33.
- `sidebar/render.rs:6534` calls `std::process::Command::new("git")` directly, bypassing `wt-core`
  entirely.
- Several `render.rs` files (`settings/render.rs` alone has 25) call `cx.background_spawn`/`cx.spawn`
  directly around adapter calls, meaning the offload-to-background decision is duplicated ad hoc at
  every call site instead of living in one place.

This works today because `crates/app` is the only consumer of `wt-core`. It becomes a real problem
the moment a second consumer (`crates/jerry-cli`) exists: any behavior implemented as "whatever the
render function happens to do around the `wt_core::` call" isn't available to the CLI, and any bug
fixed in the view's copy has to be separately remembered in the CLI's.

## Decision

Render code (`render.rs`, and generally anything that returns `impl IntoElement` or implements
`Render`) may only:

1. Read state already held on `AdeApp` (or its future per-feature successors), and
2. Dispatch a `Command` or `Query` (see [`0002-command-query-core.md`](./0002-command-query-core.md))
   and render the outcome.

It may never call `wt_core::`, `pty_core::`, `lsp_core::`, or `std::process::Command` directly. The
background-spawn decision moves into the dispatch layer, once, instead of being repeated at every
call site.

## Consequences

- This is currently violated at scale (204 direct adapter references across `render.rs` files, per
  `.claude/conventions-baseline.json`) and is **not** retroactively fixed by this ADR. It's the
  target; the gap is tracked as GitHub issues (see `docs/architecture/overview.md`'s "what must
  move" section).
- New render code written from this point on must not add new adapter calls — that part is
  effective immediately, reviewed against in `implement`/`review`/`rust-standards`, **and now
  mechanically checked**: `.claude/hooks/check-conventions.sh` greps every `render.rs` file for
  `wt_core::`/`pty_core::`/`lsp_core::`/`process::Command::new` and fails (locally, in the
  pre-commit hook, and in CI) if the count exceeds the checked-in baseline. This is a textual
  ratchet, not a type-aware lint — it can't tell a real violation from a string literal that
  happens to contain `wt_core::`, but it needs no glob-import cleanup first and catches the actual
  failure mode (a new call added where the count used to be lower) today.
- A full, type-aware `clippy::disallowed-methods` entry naming `wt_core`/`pty_core`/`lsp_core`
  paths, scoped to `render.rs` files, is still blocked on the glob-import cleanup: today, `use
  super::*` means a lint can't reliably tell which module a symbol resolved from. That cleanup is
  tracked as its own issue; the ratchet above is the interim mechanical backstop until it lands.
