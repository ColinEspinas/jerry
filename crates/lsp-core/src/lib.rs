//! `lsp-core`: a Language Server Protocol client, built to spawn and drive `rust-analyzer` for
//! Surface C's File view (`design_handoff_jerry_ade/README.md`'s "Language server UI"
//! subsection, *Diagnostic* state - see `crate::diagnostics_view` in the `app` crate for how a
//! `PublishDiagnosticsParams` becomes rendered UI).
//!
//! ## Scope decision: this crate, not a module of `app`
//!
//! Mirrors `pty-core`'s own split from `app` (see that crate's docs): the process-spawn/
//! transport-framing/handshake-correlation logic here has no dependency on GPUI at all, and
//! keeping it a separate crate means its own tests (protocol framing, process lifecycle, the
//! rust-analyzer end-to-end handshake) run as plain `cargo test -p lsp-core`, with no GPUI
//! window/test harness involved.
//!
//! ## Not a pty
//!
//! Unlike every other long-lived child process this workspace manages (`claude`, `codex`, a
//! shell - all via `pty-core`), `rust-analyzer` is spawned as a plain `std::process::Command`
//! child with `Stdio::piped()`, deliberately **not** through `pty-core`/a pseudo-terminal. A
//! pty's line discipline (cooked-mode processing: `\n` → `\r\n` translation, signal characters,
//! etc.) actively corrupts JSON-RPC's `Content-Length`-framed byte stream - the declared byte
//! count would stop matching what actually arrives once the pty has rewritten some of those
//! bytes. LSP servers are designed to be driven over plain, unmodified pipes for this reason.
//! See `crate::client`'s docs for the process spawn/kill implementation this leads to.
//!
//! ## `lsp-types`: crates.io crate, not `vendor/zed`'s fork
//!
//! Every LSP protocol type used here (`InitializeParams`, `PublishDiagnosticsParams`,
//! `Position`, `Range`, `Diagnostic`, `DiagnosticSeverity`, `Uri`, ...) comes from the
//! independently-maintained `lsp-types` crate on crates.io (MIT-licensed;
//! <https://crates.io/crates/lsp-types>), re-exported here as [`lsp_types`] so downstream
//! crates (the `app` crate's diagnostics rendering) reference the exact same version rather
//! than adding a second, independently-resolved `lsp-types` dependency. This is deliberately
//! **not** `vendor/zed`'s own git fork of `lsp-types` (`vendor/zed/Cargo.lock` pins
//! `lsp-types 0.95.1` from `git+https://github.com/zed-industries/lsp-types?rev=f4dfa89a...`,
//! GPL-3.0-or-later) - see this crate's `Cargo.toml` for the version/licensing reasoning.
//!
//! JSON-RPC 2.0's own message *framing* (`Content-Length: N\r\n\r\n<json>`) is not something
//! `lsp-types` defines at all (it only defines the request/notification payload shapes) - see
//! [`transport`] for that hand-rolled piece.

// `.expect()`/`.unwrap()` are the documented, accepted pattern in test modules (see
// `CLAUDE.md`'s Rust standards) - only production code is held to `clippy::unwrap_used`/
// `expect_used`.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod client;
// Unix-only: `crate::proc`'s `/proc`-walk and `nix` signal calls are unix-specific end to
// end (`nix` itself is only a dependency at all under `[target.'cfg(unix)'.dependencies]`
// in `Cargo.toml`) - see that module's own doc comment for the real, functional Windows
// equivalent `client.rs` uses instead (`std::process::Child::kill()`).
#[cfg(unix)]
mod proc;
mod transport;

pub use client::{
    default_workspace_configuration, ClientUpdate, LspClient, LspError, ServerSpawnConfig,
    WorkspaceConfigFn,
};
/// Re-exported so callers (the `app` crate) depend on exactly this crate's resolved
/// `lsp-types` version rather than adding a second, potentially-drifting direct dependency -
/// see this module's own docs.
pub use lsp_types;

// `transport` is intentionally not re-exported: it's an internal wire-format detail of
// `LspClient`, not part of this crate's real public surface. Its own unit tests (pure,
// non-process-spawning encode/decode coverage) live alongside it in `src/transport.rs`.
