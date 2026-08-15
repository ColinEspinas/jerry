# 0001: Core crates stay free of `gpui`

## Status

Accepted.

## Context

`wt-core`, `pty-core`, and `lsp-core` were already built with no `gpui` dependency — verified: the
only two occurrences of the string `gpui` in those three crates are comments, one explaining a
version-pin decision, the other noting that a real GPUI fake-clock test isn't possible without one.
This wasn't written down anywhere, so it was one dependency addition away from silently breaking.

## Decision

No crate other than `crates/app` (and the planned `crates/jerry-cli`, which must never gain one
either) may depend on `gpui` or `gpui_platform`. This is the foundation the rest of the target
architecture (see [`../architecture/overview.md`](../architecture/overview.md)) is built on: it is
what makes a headless CLI over the same domain logic possible at all.

## Consequences

- A PR adding `gpui` to `wt-core`, `pty-core`, or `lsp-core`'s `Cargo.toml` is a hard reject, not a
  design discussion.
- Any type that needs to cross from a core crate into `crates/app` and back must do so through plain
  data (structs, enums) — never a `gpui::Context`, `Window`, or similar. `crates/app`'s own
  `work_surface/agents.rs` violates this today by taking `Context<AdeApp>` directly in
  agent-lifecycle methods; that's tracked as follow-up work, not retroactively blessed by this ADR.
- `lsp-core`'s one cross-crate dependency (`pty-core`, for `resolve_on_path`) stays a path
  dependency between two gpui-free crates — that pattern is fine and doesn't need repeating here.
