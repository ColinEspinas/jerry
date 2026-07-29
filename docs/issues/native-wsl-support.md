# Native WSL support on Windows - full feature parity

## Summary

When ADE runs natively on Windows, it should support WSL-hosted git worktrees with **full
feature parity** - file tree, diff view, real in-app text editing (Revision R8.5), LSP
integration, and real terminal/agent sessions all need to genuinely work for a WSL-hosted
worktree, not just terminal/agent session spawning. This was explicitly decided after an
initial research pass proposed a narrower "terminal/agent sessions only" scope as an honest
fallback - that narrower scope was explicitly rejected: full parity is the real goal.

Cannot be tested on real Windows in this sandbox (Linux-only) - verification will follow the
same honest-disclosure discipline Revision R11 established: careful reasoning against real
documented APIs plus whatever cross-target type-checking is possible, clearly distinguished
from what remains genuinely unverified until real Windows hardware or CI is available.

## What VS Code's real Remote-WSL does (verified against Microsoft's own current docs)

- **Split client/server model.** The VS Code UI/client process stays on Windows; a real VS
  Code Server runs *inside* the WSL distro. "Any VS Code operations you perform in this
  window will be executed in the WSL environment, everything from editing and file
  operations, to debugging, using terminals, and more."
  ([code.visualstudio.com/docs/remote/wsl](https://code.visualstudio.com/docs/remote/wsl))
- Everything execution-heavy - file I/O, the integrated terminal, language servers, linters,
  debuggers, **git** - runs inside the distro against Linux-installed tooling, not reaching
  across from Windows.
- Path handling (Windows `C:\...` vs. WSL `/home/...`, `/mnt/c/...`) is handled
  automatically for the user, not exposed as a manual step.

This is a genuinely large architecture (a persistent server process, an extension-host-
inside-WSL model, a full remote-development protocol) - not something to fully clone, but
real, credible, first-party precedent for the one architectural decision below that matters
most.

## The settled architecture decision: execute inside the distro, don't reach across UNC

A second, targeted research pass settled the single most consequential open question:
should git and LSP operations for a WSL-hosted worktree execute as real processes *inside*
the distro (`wsl.exe -d <Distro> -- git ...`), or reach across the real
`\\wsl.localhost\<Distro>\...` UNC path from native Windows (which Windows genuinely
supports via ordinary file APIs)?

**Answer: execute inside the distro.** This is backed by real, credible, cited evidence, not
guesswork:

- **Microsoft's own documentation** states WSL2 cross-filesystem access is categorically the
  weak point: the "Performance across OS file systems" row is checked for WSL1 and
  unchecked for WSL2, with the explicit statement *"if you are using Windows applications to
  access Linux files, you will currently achieve faster performance with WSL 1"* than WSL2,
  and a direct recommendation to store project files on the same OS as the tools working on
  them. ([Comparing WSL Versions - Microsoft Learn](https://learn.microsoft.com/en-us/windows/wsl/compare-versions))
  Cross-boundary access (either direction) goes over the **9P protocol** through a Hyper-V
  socket, each message capped at 64KB, with cost dominated by *operation count* - exactly
  the "many small file stat/reads" access pattern git and LSP servers use.
- **Real, measured precedent**: GitHub Desktop's own maintainers benchmarked Windows
  `git.exe` operating on a WSL2-hosted repo across this exact boundary - `git status` 2-6s
  (vs. 100-300ms expected), `git log` 1-4s, `git fetch` 3-10s, a **10-20x slowdown** -
  and fixed it by routing git subcommands through `wsl.exe -e git` instead, i.e. the same
  "execute inside the distro" pattern recommended here.
  ([desktop/desktop#22044](https://github.com/desktop/desktop/issues/22044))
- **A real, currently-open correctness bug, not just a performance concern**, was found in
  `gix` itself - the pure-Rust git implementation `wt-core` already depends on for its real
  read path. [`gitoxide#2067`](https://github.com/GitoxideLabs/gitoxide/issues/2067)
  documents that gix's trust/ownership check (`is_path_owned_by_current_user`, which calls
  the Win32 `GetNamedSecurityInfoW` API) does not work correctly against UNC paths -
  explicitly including `\\wsl.localhost\...` paths - causing real repos at those paths to be
  mistrusted/rejected without a `safe.directory` workaround. This was verified directly
  against this project's own cached dependency source (`gix-sec-0.10.12/src/identity.rs`,
  the exact real code path `wt-core`'s `gix` dependency would hit doing repository discovery
  against a WSL-hosted path) - not a hypothetical, a real bug in a real dependency this app
  ships.
- **VS Code's own real architecture is genuine, confirmed precedent**: git and language-
  server operations happen inside the distro's own server process, not across a shared
  filesystem view - Microsoft's own compare-versions doc explicitly connects Remote-WSL's
  design to avoiding exactly this cross-filesystem performance cost.

**Practical implication**: for a WSL-hosted worktree, `wt-core`'s real git operations (both
the `gix` read path and the real `git` CLI write path) and any real LSP server spawned for
that worktree need a real "execute this inside distro X" invocation wrapper
(`wsl.exe -d <Distro> -- <command>`), with results/output piped back to the native Windows
ADE process - closer to VS Code's own real client/server boundary (a thin RPC/stdio
channel) than "point `PathBuf` at a UNC path and reuse existing code unchanged."

Lighter, latency-tolerant, low-frequency operations - opening a single file into the real
text editor buffer (Revision R8.5), listing a directory for the file tree - can reasonably
stay on direct native Windows file I/O against the `\\wsl.localhost\...` UNC path, which
Windows genuinely supports via ordinary file APIs. Caveat: even this narrower path may need
a `safe.directory`-style exception if `gix` touches it anywhere, given the confirmed trust-
check bug above.

## Real command-line surface (verified against Microsoft Learn)

- **List installed distros**: `wsl --list --verbose` (`wsl -l -v`) - name, running/stopped
  state, WSL version, default marked with an asterisk. No documented `--json` output mode -
  a builder should treat this as text-parsing with real fragility, not a stable structured
  API.
- **Run inside a specific distro**: `wsl --distribution <Name> --user <User>` (`-d`, `-u`).
  The exact `wsl -d <Distro> -- <command>` combination (with a literal `--` separator) is
  **not directly confirmed** in Microsoft's fetched docs - plausible and standard `wsl.exe`
  usage, but needs a real smoke-test on real Windows before a builder relies on it.
- **Query/set default distro**: `wsl --set-default <Name>`; `wsl --status` reports the
  current default (no separate `--get-default` flag documented).
- **`wslpath`**: real, but runs *inside* the distro, not on the Windows side - a native
  Windows process can't shell out to it directly; would need `wsl.exe -d <Distro> wslpath
  ...` (an extra process hop) or a real, independent `C:\X\...` <-> `/mnt/x/...` mapping
  implemented directly in Rust (a simple, well-known transform, not itself risky).

## What this codebase already has, and the real extension points

- `crates/pty-core/src/lib.rs` - the real spawn primitive (`SpawnOptions`/`spawn`) already
  has real `#[cfg(windows)]` branching from Revision R11, narrowly scoped to making the
  *native* Windows spawn path correctly compile/behave (ConPTY reader-thread semantics, a
  narrower `kill()` that can't reach a process tree without job objects this project's
  no-`unsafe` rule forbids attempting). None of this Windows code has ever run on real
  Windows - the crate's own doc comment says so explicitly. No WSL-awareness exists here at
  all yet. A "spawn into WSL distro X" path fits the existing `SpawnOptions` shape without
  structural change - `wsl.exe` becomes the real `program`, the real shell/agent binary
  becomes an argument to it - but `cwd` (validated via a native `is_dir()` call) would need
  to be handled differently, since the working directory *inside* the distro can't be
  expressed through `SpawnOptions::cwd` as currently used.
- `crates/app/src/env_info.rs` (Revision R6) is the **wrong direction** and must not be
  reused/extended: it detects whether *ADE itself* is running inside WSL (relevant to this
  project's own Linux/WSL2 dev environment), not whether WSL distros exist alongside a
  *native Windows* ADE process. A real, new module is needed for the latter.
- `crates/app/src/sessions.rs`'s `SessionKind` (`Shell | Claude | Codex`) should **not** get
  new WSL-specific variants (that would wrongly conflate agent identity with execution
  environment, forcing a combinatorial explosion like `ClaudeInWsl`/`CodexInWsl`). The clean
  extension point is a new, orthogonal "environment"/"execution target" concept feeding
  `TerminalSpec`/`SpawnOptions`, most naturally a per-worktree or global setting.
  `TerminalSpec::shell` (`terminal_pane.rs`) also currently hardcodes reading `$SHELL`
  (Unix-only) with a `/bin/bash` fallback (also Unix-only) - a pre-existing gap, independent
  of WSL, worth fixing alongside this work.
- `crates/app/src/root/settings_render.rs`'s Settings General page has an existing "Default
  environment" row - but it's currently a **read-only display** (the same live-detection
  chip used in the status bar/terminal footer), not a selector. A real "choose WSL distro
  for new sessions" setting needs new persisted state in `settings_store.rs`'s `Settings`
  struct (which has no path/environment-selection field today) and a new, real UI control.

## Path-handling complexity (real, non-trivial)

This app uses `PathBuf` throughout as a hard project rule. `PathBuf` on Windows is
Windows-path-semantic with no concept of a distinct WSL-side POSIX path. If a worktree's
files genuinely live inside the WSL2 filesystem (which Microsoft's own docs recommend for
performance, rather than the cross-mounted `/mnt/c`), a native Windows ADE process can't
reach them via ordinary local file APIs at all except through the `\\wsl.localhost\...` UNC
path - meaning the real question isn't "translate the path string," it's "decide, per
session/worktree, whether this worktree's entire file surface is reachable by native Windows
file I/O, and if so, at what real performance/correctness cost" (see the settled
architecture decision above). This needs to be threaded through worktree discovery, file
tree loading, diff computation, and terminal-link-open - a real, meaningfully-sized concern,
not a config tweak.

## Flagged gaps requiring real Windows verification (cannot be resolved in this sandbox)

- The exact `wsl -d <Distro> -- <command>` flag syntax.
- No direct benchmark found for an LSP server specifically operating over the
  `\\wsl.localhost\...` boundary - inferred from the general 9P cost model and rust-
  analyzer's own known sensitivity to filesystem stat/read volume, not directly measured.
- Whether the confirmed `gix` UNC trust-check bug is actually triggered by this app's real,
  specific usage pattern (repository discovery/trust resolution), or only certain code paths
  within it.
- `wsl --list --verbose`'s real output format/columns are documented in prose, not a fixed
  schema - real parsing fragility risk.

## Verification

Same discipline as every other revision: builder -> independent verification -> adversarial
checker -> fix round -> commit -> BUILD-LOG entry, plus the honest-disclosure discipline
Revision R11 established for anything that can't be verified on real hardware in this
sandbox. Given the real scope (distro detection + WSL-aware session spawning, a git-execute-
inside-distro wrapper for `wt-core`, an LSP-execute-inside-distro wrapper, settings
integration, and the file-tree/editor UNC-path exception), this should be split into
sequential sub-phases rather than one single dispatch, the same way other large revisions in
this project's history (R4, R9, R8.5) were split into lettered sub-phases.

## References

- BUILD-LOG.md's "Revision R6" and "Revision R11" entries (environment detection, real
  cross-platform build support this work extends)
- `crates/pty-core/src/lib.rs`, `crates/app/src/env_info.rs`, `crates/app/src/sessions.rs`,
  `crates/app/src/root/settings_render.rs`, `crates/app/src/settings_store.rs`
