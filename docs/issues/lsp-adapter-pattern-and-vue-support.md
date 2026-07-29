# Generalize the LSP client architecture: adapter/facade pattern + real Vue support

## Summary

Revision R8 generalized this app's LSP client beyond rust-analyzer to real, live-tested
TypeScript and Python support. Vue was investigated and deliberately deferred rather than
faked: the real, installed `@vue/language-server` crashes computing diagnostics for any
`.vue` file, because its default "hybrid mode" expects a companion `typescript-language-server`
process (loaded with `@vue/typescript-plugin`) to answer a real `tsserver/request` custom LSP
method it sends — and nothing in this app's current one-server-per-file architecture
coordinates that second process.

This is exactly how VS Code's own official Vue extension handles it for real: spawn both
servers, and have the **client** recognize that specific custom LSP method and forward it to
the companion server, then route the response back. That's a real, legitimate, documented
requirement of Vue's hybrid mode — not incidental complexity to work around.

## Motivating problem

- `crate::language`'s registry (built in R8) is "one file extension → one spawned server."
  It already carries real per-language spawn config, `language_id`, `initializationOptions`,
  and per-server `workspace/configuration` response logic — a partial step toward a real
  strategy/adapter pattern, just not fully generalized.
- `crate::root::lsp`'s client map is keyed `(PathBuf, binary)`, with fully independent
  `LspClient` instances and no coordination channel between them.
- Vue's real requirement — one file needing two coordinated server connections, with specific
  message forwarding between them — doesn't fit either of these today.

## Proposed design

Two real, standard patterns, combined:

1. **Strategy/Adapter pattern for per-language behavior.** Generalize `crate::language`'s
   registry entries into a real `LanguageAdapter`-shaped interface (trait or enum, whichever
   fits this codebase's existing conventions) that owns everything server-specific in one
   place: spawn config, `initializationOptions`, `workspace/configuration` response logic
   (already exist), plus a new capability — whether this language needs a companion server,
   and which messages get forwarded where.

2. **Facade/Coordinator pattern for multi-server languages.** A new `LspConnection`-style
   facade in `crate::root::lsp` that wraps either a single `LspClient` (today's rust-analyzer/
   TypeScript/Python case) or a primary+companion pair (Vue's case), presenting the identical
   external interface (`request<R>`, `notify<N>`, diagnostics-for-path) either way — so
   `diagnostics_view`/`hover_view`/`completion_view`/`code_surface` never need to know or care
   whether "the LSP for this file" is backed by one process or two.

## Scope

1. **Cheap investigation first**: verify whether the currently-installed
   `@vue/language-server@3.3.8` still supports a real legacy "takeover mode" (a single
   monolithic server, no companion process) via some real init option. If it still works,
   real Vue support might not need the bigger pattern immediately — verify empirically
   (spawn it for real), don't assume either way. If takeover mode technically works but is a
   deprecated/unmaintained path the Vue ecosystem is moving away from, prefer building the
   real adapter/facade generalization anyway — the durable, forward-looking architecture,
   not a legacy shortcut likely to be removed later.
2. Build the real adapter/facade generalization, favoring genuine architectural cleanliness:
   no ad-hoc Vue-specific branching bolted onto the general LSP request/response paths, no
   shortcuts that would make a third future multi-server language harder to add than the
   second one was.
3. Real Vue support end-to-end (spawn, real diagnostics, real hover) as the concrete proof
   the pattern works — live-tested against the real installed servers (rust-analyzer,
   typescript-language-server, pyright-langserver, vue-language-server), the same real
   testing discipline Revision R8 established for the other three languages.
4. Confirm the generalization is a genuine improvement, not premature abstraction: rust-
   analyzer/TypeScript/Python's existing real behavior must be provably unchanged (real
   regression tests), and the new adapter/facade shape should only be as complex as Vue's
   real, concrete need actually requires.
5. Real performance care: the message-forwarding path between a primary and companion
   server sits on the hot path of every completion/hover/diagnostic round-trip for Vue
   files — measure it, don't just make it correct. The facade's dispatch (single-server vs.
   primary+companion) must be a cheap, real branch that adds no overhead to the
   already-working rust-analyzer/TypeScript/Python path.

## Out of scope

- Any language beyond the four already real/targeted (Rust, TypeScript, Python, Vue). Go
  stays detection-only (already real, from Revision R3/R8).
- Speculative generalization for hypothetical future multi-server languages beyond what
  Vue's real, concrete need requires.

## Verification

Same discipline as every other revision in this project: builder → independent verification
→ adversarial checker (this touches shared LSP infrastructure underneath the already-working
rust-analyzer/TypeScript/Python integrations, so full adversarial review is warranted) → fix
round → commit → BUILD-LOG entry.

## References

- `crates/app/src/language.rs` (Revision R8's canonical language registry)
- `crates/app/src/root/lsp.rs` (client lifecycle/map)
- `crates/lsp-core/src/client.rs` (generic JSON-RPC request/notify mechanism)
- BUILD-LOG.md's "Revision R8" entry (Vue deferral, with the exact crash trace and the
  corrected — initially backwards — `--tsdk` evidence)
