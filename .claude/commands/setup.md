---
description: One-shot contributor bootstrap - toolchain, system deps, and rtk's token-saving hook
model: haiku
---

Get a fresh checkout ready to build, then wire up optional tooling. Report what was checked and
what (if anything) still needs manual action — don't silently skip a failed check.

## 1. Toolchain

```sh
rustc --version   # should report 1.95.0, matching rust-toolchain.toml
```

If it doesn't match, `rustup` picks up `rust-toolchain.toml` automatically on the next `cargo`
invocation in this directory — nothing to install by hand unless `rustup` itself is missing.

Tests run under `cargo-nextest`, which this repo configures in `.config/nextest.toml` (per-test
process isolation and a two-minute per-test timeout). Unlike rtk below, it is not optional — check
for it and install it if absent:

```sh
cargo nextest --version || cargo install cargo-nextest --locked
```

## 2. System dependencies

Detect the platform and check accordingly; don't run installer commands for a platform other than
the one this is running on.

**Linux**: the packages `.github/workflows/ci.yml`'s `linux` job installs
(`build-essential clang cmake pkg-config libfontconfig-dev libvulkan1 mesa-vulkan-drivers
libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libasound2-dev`). Check with
`dpkg -s <pkg>` rather than assuming; install what's missing via `apt-get`.

**macOS**: `xcode-select -p` should print a path. If a subsequent `cargo build` fails with a Metal
shader compilation error, run `xcodebuild -downloadComponent MetalToolchain` (see `README.md`'s
macOS system-dependencies section for the full detail, including the Xcode-repair fallback).

**Windows**: no extra system packages beyond the Rust toolchain itself.

## 3. rtk (optional, saves real token spend)

[rtk](https://github.com/rtk-ai/rtk) is a CLI proxy that filters command output before it reaches
the model — useful on a codebase with a 3000+-test suite and a workspace `cargo build` that can be
verbose. It's not a build dependency; skip this step cleanly if it isn't installed rather than
treating its absence as an error:

```sh
command -v rtk && rtk init -g
```

`rtk init -g` installs rtk's own Claude Code hook globally (not vendored into this repo) and is
safe to re-run. If `rtk` isn't on `PATH`, say so and point at the project it lives at
(rtk-ai/rtk) rather than attempting to install it.

## Report

One short summary: toolchain OK/mismatch, which system deps (if any) were missing and whether they
got installed, and whether rtk's hook is now active. Not a transcript of every command run.
