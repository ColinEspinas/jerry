# Architecture Decision Records

One file per significant, durable architectural decision — not a running log (that's what
`git log` and `docs/development-workflow.md`'s process notes are for). Written once; if a later
decision changes one, the later decision is its own new ADR, and the old one's Status line is
updated to point at it rather than being edited back to "current."

Start a new one from [`template.md`](./template.md), numbered `000N` sequentially. See
[`../architecture/overview.md`](../architecture/overview.md) for how these fit into the target
architecture as a whole, and `CLAUDE.md`'s architecture section / the `architecture` skill for when
a decision is significant enough to warrant one (a new crate boundary, a new cross-cutting rule, a
reversal of an earlier ADR — not "which function does this go in," which is just judgment applied
to an existing rule).

| # | Decision | Status |
|---|---|---|
| [0001](./0001-gpui-free-core-crates.md) | `wt-core`/`pty-core`/`lsp-core` never depend on `gpui` | Accepted |
| [0002](./0002-command-query-core.md) | Application-layer unit is a Command (mutation) or Query (read), not a loose function | Accepted |
| [0003](./0003-ui-must-not-call-adapters.md) | Render code dispatches a Command/Query; it never calls an adapter directly | Accepted — partially enforced (ratchet in `.claude/hooks/check-conventions.sh`; full type-aware lint pending the glob-import cleanup) |
| [0004](./0004-retire-build-log.md) | `BUILD-LOG.md`/`ASSESSMENT.md` retired in favor of ADRs | Accepted |
