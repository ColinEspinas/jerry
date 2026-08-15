---
name: rust-standards
description: Walk Rust code in this repo against the hard rules clippy can't check on its own - PathBuf discipline, git-as-argv, the plural.rs helper, no fake functionality, GPUI blocking-call offload, the comment rule. Use this while writing Rust in this project, not only when reviewing it afterward - run it before calling a step done, whenever the user asks if something is idiomatic or "follows our conventions", or says things like "does this match our rust standards", "check this against CLAUDE.md", "is this the right pattern here". clippy -D warnings already catches unwrap/expect/unsafe/dbg! mechanically (see CLAUDE.md and [workspace.lints]) - this skill is for the rest, the things a green clippy run doesn't prove.
---

# Rust standards

`cargo clippy -D warnings` is a real gate, but it's a mechanical one — it catches an `unwrap()` or
an `unsafe` block with no local `#[allow]`, and stops there. Most of this project's actual hard
rules (`CLAUDE.md`) live past what any lint can express: whether a path is a `PathBuf` or a
`String` that happens to compile, whether a count went through the pluralization helper, whether a
render function reached into an adapter it has no business calling. This skill is that checklist,
meant to run *while* writing code, not only as a post-hoc review gate.

## The checklist

For each item, the question isn't "does this compile" — it's "does this hold under the case that
isn't the happy path."

1. **Paths.** Is every filesystem path a `PathBuf`/`&Path`, never a `String` re-parsed at the call
   site? A path with a space or a non-UTF8 byte is the case that breaks a stringly-typed one
   silently.

2. **Git invocations.** Every mutating git operation a real argument vector — `["commit", "-m",
   msg]`, never `format!("git commit -m '{msg}'")`. A branch name or commit message containing a
   quote is the case an interpolated string gets wrong, and it's exactly the kind of input a real
   user's repo can contain.

3. **Pluralization.** Every user-visible count through `crates/app/src/root/plural.rs` —
   `plural::count(n, "file", None)`, `plural::form(n, "needs", "need")`. Grep for a raw `if n == 1`
   ternary or a hardcoded plural noun (`format!("{n} agents")`) near any count; both are the same
   bug wearing different clothes, and the single-item case is usually the common one that ships
   broken.

4. **No fake functionality.** Is every control actually wired to something real — no hardcoded data
   standing in for a live source, no simulated output, nothing that renders as done but dispatches
   nothing? If a piece genuinely isn't built yet, is that stated visibly (a real "not implemented"
   state or `todo!("unverified: ...")` with an explanation), not silently faked?

5. **GPUI blocking calls.** Every call into `wt-core`/`lsp-core` — both documented blocking — goes
   through `cx.background_spawn`/`cx.spawn`, never called directly from a render path. Does a
   spawned `Task` get cancelled when the entity it belongs to drops, rather than orphaned? Does
   `cx.notify()` fire exactly when state the render function reads actually changed?

6. **Layering.** Does render code (`render.rs`, anything implementing `Render`/`IntoElement`) call
   `wt_core::`/`pty_core::`/`lsp_core::` or shell out via `std::process::Command` directly, instead
   of dispatching a Command/Query? New code follows the target in
   `docs/architecture/overview.md`/`docs/adr/0003-ui-must-not-call-adapters.md` even though most of
   the existing codebase doesn't yet — if you're unsure whether something needs a full
   Command/Query shape, the `architecture` skill covers that decision.

7. **Comments.** Does a comment explain a non-obvious *why*, or does it restate the line below it?
   Does it narrate design history, alternatives considered, or an issue number's whole backstory —
   content that belongs in the commit body or a `docs/adr/000N-*.md` instead? `CLAUDE.md`'s comment
   rule exists because this codebase's comments were 25.7% of its lines at last measurement; new
   code is what keeps that number from climbing back up.

8. **Imports.** Any new `use super::*`? Explicit imports only — a glob is how a "pure" module ends
   up silently importing `gpui`, which is exactly what makes rule 6 unenforceable by a lint today.

## What this skill is not

It doesn't run `cargo clippy`/`cargo test` itself — that's `/check`. It doesn't decide whether new
logic needs its own crate or a Command/Query shape — that's `architecture`. It's the fast,
inline pass for the rules that are easy to forget mid-implementation because nothing red flags them
automatically.
