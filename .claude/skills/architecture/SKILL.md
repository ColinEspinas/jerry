---
name: architecture
description: Decide where new logic belongs - which crate, which layer, whether it needs to be a Command or a Query, whether it's UI-only glue - before writing it, using the dependency rule and Command/Query model in docs/architecture/. Use whenever starting work that isn't obviously confined to one file, when unsure whether something belongs in wt-core/pty-core/lsp-core vs. app, whether a new capability should be a Command/Query vs. a plain function, whether a change would violate a core crate staying gpui-free, or when the user asks "where should this live", "should this be its own crate", "does this belong in the domain or the UI layer". Not for routine same-shape-as-neighboring-code changes - this is for the moment a change doesn't have an obvious home yet.
---

# Architecture

Jerry's target shape is a dependency rule (nothing points outward) plus a Command/Query
application layer, documented in [`docs/architecture/overview.md`](../../../docs/architecture/overview.md),
[`docs/architecture/crates.md`](../../../docs/architecture/crates.md), and the reasoning behind
each rule in [`docs/architecture/decisions.md`](../../../docs/architecture/decisions.md). Most of
the existing codebase doesn't follow it yet — this skill is for deciding where *new* work should go
so it doesn't add to that gap, without pretending the whole codebase already matches the target.

## The decision, in order

**1. Is this pure domain/infrastructure logic — git, process/PTY, LSP — with no rendering
concern?** It belongs in `wt-core`, `pty-core`, or `lsp-core`, and it must never import `gpui`.
This is non-negotiable, not a style preference: see `decisions.md` §1. If the logic needs something
from GPUI's world (a `Context`, a `Window`), that's a sign it doesn't belong in a core crate at all
— find the boundary and pass plain data across it instead.

**2. Is this a mutation, or a read with no side effects, that the view needs to trigger?** Shape it
as a Command (mutation) or Query (read) — a typed input struct, a typed outcome — even if it's
sitting inside `crates/app` for now rather than fully layered out. See `decisions.md` §2 for the
shape and why a loose function doesn't give the same thing. This is what eventually lets
`crates/jerry-cli` dispatch the same action the view does — a function with an ad hoc signature
can't be dispatched generically, a Command can.

**3. Is this rendering — drawing state, wiring up interaction?** It reads state already held on
`AdeApp` (or a future per-feature successor) and dispatches a Command/Query for anything that
mutates or needs fresh data. It does not call `wt_core::`/`pty_core::`/`lsp_core::` or shell out
directly — see `decisions.md` §3. If you're touching a `render.rs` that already violates this (most
of them do today — 109 `wt_core::` references in `graph_view/render.rs` alone), don't compound it
with a new one; whether to also fix the existing violations in that file is a separate call from
whether your own change adds new ones.

**4. Does this look like it doesn't need `gpui` at all, but it's about to land inside
`crates/app`?** Check first — `crates/app` already carries roughly 24k lines with zero `gpui`
dependency that arguably shouldn't be there (`hooks/`, `provenance/`, parts of `rail/`, and more;
see `docs/architecture/overview.md`'s "what must move" section). Don't add to that pile
deliberately. If the new logic is genuinely substantial and self-contained, ask whether it should
be its own crate instead (next question) rather than another gpui-free module buried in the UI
crate by default.

**5. Does this need to be its own crate?** Only when it's gpui-free, has a real independent
identity (not just "a group of related functions"), and is large/stable enough that a crate
boundary earns its keep over just being a well-organized module. A five-function helper doesn't
need a crate. `crates/app/src/hooks/` (~5,400 LOC, a whole HTTP listener) does — it's the named
pilot extraction in `docs/architecture/overview.md`.

**A new crate is a normal, expected outcome of this decision, not an exception to ask permission
for** — `wt-core`/`pty-core`/`lsp-core` are exactly this pattern already, and the target
architecture explicitly plans a fourth (`crates/jerry-cli`). If question 5 says yes, create it:

1. `crates/<name>/Cargo.toml` and `src/lib.rs`, matching an existing core crate's shape
   (`edition = { workspace = true }`, `publish = { workspace = true }`, `license = { workspace = true }`).
2. Add `[lints]` / `workspace = true` to its `Cargo.toml` — every crate opts into
   `[workspace.lints]` individually; it isn't automatic. Skipping this is the single easiest way to
   ship a crate that silently isn't held to `unsafe_code = "deny"` / `clippy::unwrap_used`.
3. Add `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]` near the top of
   `src/lib.rs` (after any `//!` module doc, before the first `use`/`mod`) — matches the "expect()
   is fine in tests" convention every other crate already carries.
4. Add it to the root `Cargo.toml`'s `[workspace]` `members` list.
5. Never add a `gpui` dependency — see §1 above, non-negotiable regardless of what the new crate
   is for.
6. Add its Scope / Owns / Does not own entry to
   [`docs/architecture/crates.md`](../../../docs/architecture/crates.md).

Run `cargo build --workspace` and `/check` once it's wired up — a crate that isn't in `members` yet
won't be picked up by either.

## When none of this is clear yet

If a change's home is genuinely ambiguous — it could reasonably go in two different crates, or the
Command/Query shape doesn't fit cleanly — say so explicitly rather than picking silently. This is
exactly the kind of fork `plan` should surface on the issue before `implement` starts writing code.

## When the decision is genuinely new

Questions 1–5 apply an existing rule; most work is that, and stops there. Occasionally a change
needs a call none of the four entries in `docs/architecture/decisions.md` already make — a new
crate boundary, a new cross-cutting rule, a reversal of an earlier one. That's worth a new numbered
entry there, not a paragraph buried in a commit message: follow the existing entries' shape
(Status / Context / Decision / Consequences) and append it, rather than editing an old entry back to
"current." Don't reach for this for routine applications of an existing rule (`hooks/` being the
pilot extraction candidate doesn't need its own entry — it's already covered by §1/§2) — an entry
records a *decision*, not every instance of following one.

## What this skill doesn't cover

It doesn't check whether existing code already violates the target (that's `rust-standards`'s
layering check, applied to a diff), and it covers scaffolding a *new* crate but not *extracting*
one — moving a substantial existing module like `hooks/` out into its own crate, updating every
caller, is real refactor work with its own risk, not a checklist item.
