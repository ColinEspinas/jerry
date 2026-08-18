//! `lsp-core`: a Language Server Protocol client, spawning and driving `rust-analyzer`.
//!
//! Its own crate rather than a module of `app`, for the same reason as `pty-core`: no GPUI
//! dependency, so its tests run without a window harness.
//!
//! The server is a plain piped `std::process::Command` child, **not** a pty. A pty's line
//! discipline rewrites bytes (`\n` to `\r\n`, signal characters), which corrupts JSON-RPC's
//! `Content-Length` framing - the declared count stops matching what arrives.
//!
//! Protocol types come from crates.io `lsp-types` (MIT), re-exported so callers share this
//! crate's resolved version, deliberately not `vendor/zed`'s GPL fork. Framing itself is not
//! something `lsp-types` defines; see [`transport`].

// Only production code is held to `unwrap_used`/`expect_used` and the bare-`Command::new`
// ban (`clippy.toml`, GitHub issue #465); see `CLAUDE.md`.
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)
)]

mod client;
// Unix-only: the `/proc` walk and `nix` signals have no Windows equivalent, so `client.rs` uses
// `std::process::Child::kill()` there instead.
#[cfg(unix)]
mod proc;
mod transport;

pub use client::{
    default_workspace_configuration, ClientUpdate, LspClient, LspError, ServerSpawnConfig,
    WorkspaceConfigFn,
};
pub use lsp_types;

// `transport` is not re-exported: it is an internal wire-format detail of `LspClient`.
