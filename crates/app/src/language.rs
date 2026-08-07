//! The single, canonical per-extension language registry - Revision R8's consolidation of what
//! used to be four independently-maintained tables that could silently drift against each other:
//! `crate::settings::state::LSP_LANGUAGES` (the Language servers settings page),
//! `crate::sidebar::file_tree::lang_chip_for_name` (file-tree chip colors), `crate::code_surface::code_view`'s
//! `language_name_for_extension` (the File view's status-bar language label), and
//! `crate::code_surface`'s old `is_rust` boolean gate (whether to spawn/talk to an LSP
//! client at all). All four now read from [`entry_for_extension`]/[`EXTENSIONS`] below - matching
//! this codebase's Revision R5.5-established preference for one real table over several that can
//! drift, not a forced abstraction (each entry is still just a few plain fields, no trait
//! machinery).
//!
//! ## Scope: which languages actually spawn a server here
//!
//! `lsp: Some(..)` for Rust/TypeScript-family/Python/**Vue** - all real, live-tested end to end
//! (see `lsp_core::client`'s own tests for Rust, and this crate's `language::tests` /
//! `lsp::client`-adjacent integration tests for the rest). `lsp: None` for TOML/Markdown/SQL/
//! JSON/YAML/C/HTML/CSS (none ever had a server here - HTML and CSS are GitHub issue #154, whose
//! text asks for syntax support only) and Go (the user's explicit ask named TypeScript/Vue/Python, not Go; `gopls`
//! stays PATH-detection-only on the Settings page, matching its pre-existing "not installed" real
//! state there).
//!
//! ## Vue: the investigation that led to [`CompanionServer`]
//!
//! Vue was deferred through Revision R8 rather than faked, and the investigation narrative is kept
//! here because it's what the eventual design is justified by - the deferral itself is now
//! resolved (this module's [`VUE_LSP`] and [`CompanionServer`], plus `crate::lsp::client`'s own
//! `LspConnection` facade, are that resolution).
//!
//! Live-probing the sandbox's actual installed `@vue/language-server@3.3.8` (`vue-language-server
//! --stdio`, no `--tsdk`) showed it completes a real `initialize` handshake and advertises
//! `hoverProvider`/`definitionProvider`/etc., but **hard-crashes** (uncaught `TypeError: Cannot
//! read properties of undefined (reading 'protocol')` in `@vue/language-server/lib/server.js`,
//! process exit) the instant it tries to compute diagnostics for *any* `.vue` file - even one
//! with no `<script>` block at all.
//!
//! Correction (a previous version of this doc comment got the evidence backwards): that crash is
//! specifically what happens when `--tsdk` is **omitted**, not when it's passed - the server then
//! falls back to `require('typescript')`, resolving its own bundled copy, which in this
//! environment is TypeScript 7.0.2 whose restructured module layout has no `.server.protocol` on
//! the base module. [`vue_dynamic_args`] below is the real `--tsdk` resolution that avoids it.
//!
//! Legacy "takeover mode" (a single monolithic server needing no companion, historically
//! selectable via `initializationOptions.vue.hybridMode = false`) was re-checked live for this
//! revision and is genuinely **gone**, not merely deprecated: passing that flag still produces the
//! identical crash, and reading the installed `@vue/language-server@3.3.8`'s own `lib/server.js`
//! shows no `hybridMode` string anywhere in it - `getLanguageService()` unconditionally issues the
//! `tsserver` request the first time any file needs a language service, with no code path that
//! skips it. So there is no shortcut: hybrid mode's real two-process coordination is the only real
//! way to support Vue against this server.
//!
//! What hybrid mode actually needs, verified byte-for-byte against both real processes:
//! `vue-language-server` sends a custom `tsserver/request` notification asking its *client* to
//! relay a query to a companion `typescript-language-server` running `@vue/typescript-plugin`, and
//! to notify the answer back as `tsserver/response`. [`CompanionServer`] is this registry's half
//! of that (which companion, how to configure it, which methods carry the relay);
//! `crate::lsp::client`'s `LspConnection` is the half that actually performs it.
//!
//! One live-verified consequence shapes the whole design, and is worth stating plainly because it
//! looks like a bug otherwise: in hybrid mode the *split is the point*. The primary
//! (`vue-language-server`) answers Vue-specific things - a real, live-observed example being
//! template compile diagnostics (`"Element is missing end tag."`, `"Invalid end tag."`) - and
//! genuinely returns nothing for TypeScript-semantic queries, because those are the companion's
//! job: opening the same `.vue` file directly in the companion (with the plugin loaded,
//! `language_id: "vue"`) produces the real `"Type 'string' is not assignable to type 'number'."`
//! diagnostic and a real `const bad: number` hover, entirely on its own. Neither server is the
//! whole answer for a `.vue` file, which is exactly why `LspConnection` merges diagnostics from
//! both and falls back to the companion for hover.

use std::path::Path;

use gpui::Rgba;
use lsp_core::lsp_types::request::{
    Completion, GotoDefinition, HoverRequest, Request as LspRequest,
};
use lsp_core::{ServerSpawnConfig, WorkspaceConfigFn};

use crate::theme;

/// One real language server's spawn identity, shared by every [`ExtensionEntry`] that routes to
/// it (e.g. `.ts`/`.tsx`/`.js`/`.jsx` all share one [`LspIdentity`] - one real
/// typescript-language-server process per repo root, not four). Fields mirror
/// [`lsp_core::ServerSpawnConfig`] directly; [`entry_for_extension`]'s callers turn this plus a
/// real per-extension `language_id` into an owned `ServerSpawnConfig` via
/// [`server_spawn_config`].
#[derive(Debug, Clone, Copy)]
pub struct LspIdentity {
    pub binary: &'static str,
    pub args: &'static [&'static str],
    /// Built fresh per real spawn (see [`server_spawn_config`]) via `crate::root::AdeApp::
    /// ensure_lsp_client`, which builds the real `ServerSpawnConfig` (calling this fn) inside its
    /// `cx.background_executor()` task, only once it has confirmed a fresh spawn is actually
    /// needed - not eagerly on the GPUI render path. A real, possibly PATH-probing computation
    /// (Pyright's real `pythonPath` detection, see `pyright_initialization_options` below) would
    /// be too expensive to run on every repaint, which is exactly why the render path only ever
    /// looks up the cheap, static [`lsp_binary_for_extension`] instead of calling this.
    pub initialization_options: fn() -> Option<serde_json::Value>,
    pub workspace_configuration: WorkspaceConfigFn,
    /// Real, dynamic *extra* arguments appended after [`Self::args`], resolved fresh per real
    /// spawn against the actual repository being opened - [`no_dynamic_args`] (an unconditional
    /// `Ok(vec![])`) for every server whose real command line is fully static, which is all three
    /// of Rust/TypeScript/Python.
    ///
    /// Takes `repo_root` because the one real user of it genuinely needs it: Vue's `--tsdk` must
    /// point at *this project's own* TypeScript installation (see [`vue_dynamic_args`]). Fallible
    /// for the same reason [`CompanionServer::initialization_options`] is - an `Err` means a real,
    /// checked prerequisite is genuinely missing, and [`server_spawn_config`] then refuses to
    /// spawn rather than starting a process already known to crash.
    ///
    /// Same "built fresh, off the GPUI thread, only when a spawn is actually needed" contract as
    /// [`Self::initialization_options`] - it does real filesystem probing.
    pub dynamic_args: fn(repo_root: &Path) -> Result<Vec<String>, String>,
    /// The second, coordinated server this language's primary needs alongside it, if any - `None`
    /// for every single-process language (Rust/TypeScript/Python). See [`CompanionServer`].
    pub companion: Option<CompanionServer>,
}

/// A second, coordinated LSP server process some languages need alongside the primary one - see
/// this module's own top-level docs for the real, concrete reason (Vue's hybrid mode) and
/// `crate::lsp::client`'s `LspConnection` for the facade that actually drives both.
///
/// Deliberately concrete to that one real, verified protocol rather than a speculative N-server
/// framework: the relay is one request-shaped notification in, one response-shaped notification
/// out, through one `workspace/executeCommand` on the companion. A third multi-server language
/// that needed a genuinely different coordination shape should extend this against *its* real
/// requirement, not against a guess made here.
#[derive(Debug, Clone, Copy)]
pub struct CompanionServer {
    /// The second half of this companion's own `LspClientKey` in `crate::root::AdeApp::
    /// lsp_clients`, and the real `name` its `lsp_core::LspClient` reports in every log/status
    /// message. Deliberately distinct from [`Self::binary`]'s own plain name (e.g.
    /// `"typescript-language-server (vue)"` vs `"typescript-language-server"`) so a
    /// Vue-companion-flavored spawn - which carries a real, extra `@vue/typescript-plugin` in its
    /// `initializationOptions` - never collides with, or gets silently reused as, the plain
    /// TypeScript client the same repo root might independently have running for real `.ts` files.
    pub client_key: &'static str,
    pub binary: &'static str,
    pub args: &'static [&'static str],
    /// `didOpen`'s `language_id` when opening the *same* file in this companion too. The companion
    /// is a real, independent analyzer of the file, not a passive relay - see this module's
    /// top-level docs for the live-verified split that makes its own `didOpen` necessary.
    pub language_id: &'static str,
    /// Real, dynamic (PATH/filesystem-probing) companion `initializationOptions` - mirrors
    /// [`LspIdentity::initialization_options`]'s own "built fresh per real spawn, off the GPUI
    /// thread" contract, but fallible: `Err` is a genuine missing prerequisite (e.g. no real,
    /// resolvable `@vue/typescript-plugin` on disk), and spawning is refused rather than started
    /// knowing it can't do its job.
    pub initialization_options: fn() -> Result<Option<serde_json::Value>, String>,
    /// The custom notification method the **primary** sends to ask its client to relay a query to
    /// this companion. Named here, in the registry, rather than in `crate::lsp::client`, so the
    /// forwarding code stays free of any particular server's protocol vocabulary.
    pub relay_request_method: &'static str,
    /// The custom notification method the client sends **back to the primary** carrying the
    /// companion's answer.
    pub relay_response_method: &'static str,
    /// The real `workspace/executeCommand` command name this companion registers for answering a
    /// relayed query (`typescript-language-server` registers `"typescript.tsserverRequest"`
    /// specifically to make raw tsserver queries reachable over standard LSP - confirmed by
    /// reading its own `lib/cli.mjs`).
    pub relay_command: &'static str,
    /// The real `lsp_types::request::Request::METHOD` strings whose *empty* primary answer should
    /// be retried against this companion - see `crate::lsp::client::LspConnection::request`, which
    /// reads exactly this list instead of hardcoding any method of its own.
    ///
    /// Lives here, in the registry, because "which questions does the primary genuinely not answer
    /// for its own files" is a property of the specific two-server split, not of the facade: a
    /// future companion for a different language would have its own, different list, and the
    /// coordination code shouldn't need editing to accommodate it.
    pub fallback_methods: &'static [&'static str],
}

/// Which of `crate::code_surface::code_view`'s real `tree-sitter`-backed syntax highlighters applies to an
/// extension, if any - see [`ExtensionEntry::highlighter`]'s own docs for why this lives on the
/// registry itself rather than in a second, independent table.
pub type HighlighterFn = fn(&str) -> Vec<crate::code_surface::code_view::HighlightSpan>;

/// One real file extension's language identity - see this module's top-level docs.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionEntry {
    /// Lowercase, no leading dot (e.g. `"rs"`, `"tsx"`).
    pub extension: &'static str,
    pub display_name: &'static str,
    /// The real `textDocument/didOpen` `language_id` for this specific extension - even within
    /// one shared [`LspIdentity`], this varies per extension (`"typescript"` vs
    /// `"typescriptreact"` vs `"javascript"` vs `"javascriptreact"`), so it lives here, not on
    /// [`LspIdentity`].
    pub lsp_language_id: &'static str,
    pub chip_label: &'static str,
    /// `(fg, bg)`, straight from `crate::theme::lang`. Kept as the real, still-`const`-usable
    /// [`crate::theme::ColorToken`] (this whole struct lives inside the real compile-time
    /// `const` array [`EXTENSIONS`], which a runtime-resolved `Rgba` couldn't) - resolved to a
    /// concrete `Rgba` only where actually consumed (`chip_for_extension`).
    pub chip_colors: (theme::ColorToken, theme::ColorToken),
    pub lsp: Option<LspIdentity>,
    /// The Settings "Language servers" page's own real row for this language - `Some` for
    /// exactly the five extensions that page shows one row per (`rs`/`ts`/`vue`/`py`/`go`, one
    /// row per *family*, not per extension - `.tsx`/`.js`/`.jsx` share TypeScript's row), `None`
    /// for every other entry (TOML/Markdown/SQL never had a server; `.tsx`/`.js`/`.jsx` aren't a
    /// second, redundant TypeScript row). Carries the real detection binary independently of
    /// [`Self::lsp`] - Go is a real, `$PATH`-detectable binary this app deliberately doesn't spawn
    /// a live client for (see this module's top-level docs), so the Settings page still needs a
    /// binary name for it even though its [`Self::lsp`] is `None`. This row also names only the
    /// *primary* binary: Vue genuinely runs a second, companion process too (see
    /// [`CompanionServer`]), which the page deliberately doesn't show a separate row for - it is
    /// an implementation detail of Vue support, not a language the user picks.
    pub settings_row: Option<SettingsLspRow>,
    /// Which real `crate::code_surface::code_view` highlighter function parses this extension's real syntax -
    /// `None` for an extension with no `tree-sitter` grammar wired at all (TOML/Markdown/SQL, and
    /// Vue - there is no `.vue` grammar dependency in this workspace, which is entirely unrelated
    /// to and independent of whether [`Self::lsp`] spawns a real server for it, and it does). Previously `crate::code_surface::code_view::load_file` maintained its own
    /// second, independent extension -> highlighter `match` that this same registry knew nothing
    /// about - a real, live gap where a new registered language could silently render as plain
    /// text with no compile error or test failure to catch it. This field, plus
    /// `crate::code_surface::code_view::highlighter_for_extension` reading from it, is that gap closed: one
    /// real table, not two that can drift.
    pub highlighter: Option<HighlighterFn>,
}

/// See [`ExtensionEntry::settings_row`].
#[derive(Debug, Clone, Copy)]
pub struct SettingsLspRow {
    pub binary: &'static str,
    /// Generic descriptive copy, not a live count - see `crate::settings::state::LspLanguage::note`'s
    /// own docs (this struct is `crate::settings::state::LSP_LANGUAGES`' one real source now).
    pub note: &'static str,
    /// The real, official install/docs page for this server - what the Settings -> Language
    /// servers page's "Install" action (a genuinely `not installed` row only, see
    /// `crate::settings::state::LspRow::is_ready`) opens in the user's default browser via
    /// `crate::settings::widgets::open_command_for`. Deliberately the server's own official
    /// repo/README or docs site, not a third-party aggregator - each one was checked against the
    /// real, current page before being added here (see this app's task tracker for the real
    /// per-server verification: `rust-analyzer`'s own manual's binary-install chapter,
    /// `typescript-language-server`'s own GitHub README, the Vue core team's own
    /// `vuejs/language-tools` README, Pyright's own `docs/installation.md`, and `gopls`'s own
    /// `go.dev/gopls` documentation).
    pub install_url: &'static str,
}

fn no_initialization_options() -> Option<serde_json::Value> {
    None
}

/// The shared [`LspIdentity::dynamic_args`] for every server whose real command line is fully
/// static - an unconditional `Ok(vec![])`, so its real spawn `args` are exactly
/// [`LspIdentity::args`] and nothing else. Mirrors [`no_initialization_options`]'s own shape.
fn no_dynamic_args(_repo_root: &Path) -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

/// Vue's real, required `--tsdk` argument: the directory holding a **classic-layout** TypeScript's
/// own `lib/typescript.js`. Resolved from the real project being opened, existence-checked, with
/// an honest `Err` (and so a refused spawn, surfaced as a real `LspClientState::Failed` message)
/// when there genuinely isn't one - never a fabricated path, and never a spawn this app already
/// knows will crash.
///
/// Project-local (`<repo_root>/node_modules/typescript/lib`) is the correct real signal, not a
/// global npm root: `--tsdk` is what supplies the type information for *this* project's own code,
/// so this project's own installed TypeScript is exactly what it should point at.
///
/// The existence check is on `lib/typescript.js` **specifically**, not on the directory: that
/// file's presence is the real, verifiable distinction between a classic-layout TypeScript (5.x,
/// which works) and the newer restructured layout (7.x, which has no `.server.protocol` on its
/// base module and reproduces the exact crash this flag exists to avoid). A directory-only check
/// would happily pass for the version that crashes.
fn vue_dynamic_args(repo_root: &Path) -> Result<Vec<String>, String> {
    let tsdk = repo_root
        .join("node_modules")
        .join("typescript")
        .join("lib");
    if !tsdk.join("typescript.js").is_file() {
        return Err(format!(
            "Vue support needs a project-local `typescript` (run `npm install typescript`) \
             providing node_modules/typescript/lib/typescript.js; none found under {}",
            repo_root.display()
        ));
    }
    // `.to_str()`, not `.to_string_lossy()` - the same discipline
    // [`pyright_initialization_options`] already follows: a real non-UTF-8 path handed over
    // lossily would point the server at a directory that doesn't exist, which is worse than an
    // honest refusal.
    let tsdk = tsdk.to_str().ok_or_else(|| {
        format!(
            "the project-local TypeScript path under {} is not valid UTF-8, so it cannot be \
             passed as `--tsdk`",
            repo_root.display()
        )
    })?;
    Ok(vec![format!("--tsdk={tsdk}")])
}

/// The real `@vue/typescript-plugin` directory, resolved from the actually-installed
/// `vue-language-server` binary rather than guessed - `@vue/typescript-plugin` is a real
/// `dependencies` entry in `@vue/language-server`'s own `package.json`, so npm always installs it
/// as a nested `node_modules/@vue/typescript-plugin` under that package's root. That makes this a
/// deterministic filesystem walk, not a heuristic:
///
/// 1. `$PATH`-resolve `vue-language-server` (the same [`pty_core::resolve_on_path`] helper
///    [`pyright_initialization_options`] already uses).
/// 2. `canonicalize` it, following through any shim/symlink (e.g. mise's) onto the real script.
/// 3. The package root is two directories up (`<root>/bin/vue-language-server.js`).
///
/// Returns the **package root**, which is what the companion's plugin `location` field wants:
/// `typescript-language-server` forwards that value to `tsserver` as a `--pluginProbeLocations`
/// entry, and tsserver resolves the plugin by Node module resolution *from* that directory - so it
/// must be a directory containing `node_modules/@vue/typescript-plugin`, not the plugin's own
/// directory. Verified live against both real processes.
fn vue_typescript_plugin_location() -> Result<String, String> {
    let binary = pty_core::resolve_on_path("vue-language-server").ok_or_else(|| {
        "Vue's TypeScript companion needs `vue-language-server` on PATH to locate its own \
         bundled `@vue/typescript-plugin`; none found"
            .to_string()
    })?;
    let script = std::fs::canonicalize(&binary).map_err(|err| {
        format!(
            "could not resolve the real `vue-language-server` script behind {}: {err}",
            binary.display()
        )
    })?;
    let package_root = script
        .parent()
        .and_then(|bin_dir| bin_dir.parent())
        .ok_or_else(|| {
            format!(
                "the real `vue-language-server` script at {} is not inside a `<package>/bin/` \
                 directory, so its package root cannot be resolved",
                script.display()
            )
        })?;
    let plugin_dir = package_root
        .join("node_modules")
        .join("@vue")
        .join("typescript-plugin");
    if !plugin_dir.is_dir() {
        return Err(format!(
            "Vue's TypeScript companion needs a real `@vue/typescript-plugin`; none found at {}",
            plugin_dir.display()
        ));
    }
    package_root.to_str().map(str::to_string).ok_or_else(|| {
        format!(
            "the `@vue/language-server` package root at {} is not valid UTF-8, so it cannot \
                 be passed as a plugin location",
            package_root.display()
        )
    })
}

/// The companion's real `initializationOptions`: `typescript-language-server`'s own documented
/// `plugins` mechanism, loading the real `@vue/typescript-plugin` and registering it for the
/// `"vue"` language. This is what makes the companion able to answer both the relayed
/// `tsserver/request` queries *and* real, independent diagnostics/hover for `.vue` files.
fn vue_companion_initialization_options() -> Result<Option<serde_json::Value>, String> {
    let location = vue_typescript_plugin_location()?;
    Ok(Some(serde_json::json!({
        "plugins": [{
            "name": "@vue/typescript-plugin",
            "location": location,
            "languages": ["vue"],
        }],
    })))
}

/// Pyright expects a real, non-empty `initializationOptions` block up front (unlike
/// rust-analyzer/typescript-language-server, which behave fine with none) - see this crate's
/// `lsp-core` generalization docs. Real `pythonPath` detection via `pty_core::resolve_on_path`
/// (the same real `$PATH` search `crate::settings::state::detect_lsp_rows` already uses), not a
/// hardcoded guess; `None` (omitted from the JSON entirely, not a fabricated empty string) if no
/// real `python3`/`python` is found, so Pyright falls back to its own default resolution instead
/// of being pointed at an interpreter that doesn't exist.
fn pyright_initialization_options() -> Option<serde_json::Value> {
    // `.to_str()`, not `.to_string_lossy()` - a real non-UTF-8 path would otherwise be silently,
    // lossily corrupted into a `pythonPath` string that points nowhere real. Following the same
    // "omit the key entirely on a real absence" pattern already used just below for "no
    // python3/python found at all": a non-UTF-8 resolved path is treated the same honest way as
    // no resolved path, rather than handed to Pyright as a wrong value.
    let python_path = pty_core::resolve_on_path("python3")
        .or_else(|| pty_core::resolve_on_path("python"))
        .and_then(|path| path.to_str().map(str::to_string));

    let analysis = serde_json::json!({
        "autoSearchPaths": true,
        "useLibraryCodeForTypes": true,
        "diagnosticMode": "openFilesOnly",
    });
    let mut python = serde_json::Map::new();
    if let Some(python_path) = python_path {
        python.insert(
            "pythonPath".to_string(),
            serde_json::Value::String(python_path),
        );
    }
    python.insert("analysis".to_string(), analysis);
    Some(serde_json::json!({ "python": python }))
}

/// Pyright's real `workspace/configuration` answers - see `lsp_core::client`'s generalization
/// docs for why a bare `null` (this client's old, rust-analyzer-only behavior) is actively unsafe
/// for Pyright specifically: a `null` reply for the `"python"` section reads to Pyright as "no
/// settings", which can leave it on a stale/default interpreter rather than a real one. Mirrors
/// [`pyright_initialization_options`]'s own shape so the two never independently drift - both
/// answer the same real interpreter/analysis settings, just delivered through the two different
/// real channels Pyright asks over (once at startup via `initializationOptions`, and again live
/// via `workspace/configuration` whenever it wants to re-check).
fn pyright_workspace_configuration(section: Option<&str>) -> serde_json::Value {
    // `.to_str()`, not `.to_string_lossy()` - a real non-UTF-8 path would otherwise be silently,
    // lossily corrupted into a `pythonPath` string that points nowhere real. Following the same
    // "omit the key entirely on a real absence" pattern already used just below for "no
    // python3/python found at all": a non-UTF-8 resolved path is treated the same honest way as
    // no resolved path, rather than handed to Pyright as a wrong value.
    let python_path = pty_core::resolve_on_path("python3")
        .or_else(|| pty_core::resolve_on_path("python"))
        .and_then(|path| path.to_str().map(str::to_string));
    let analysis = serde_json::json!({
        "autoSearchPaths": true,
        "useLibraryCodeForTypes": true,
        "diagnosticMode": "openFilesOnly",
    });
    match section {
        Some("python") => {
            let mut python = serde_json::Map::new();
            if let Some(python_path) = python_path {
                python.insert(
                    "pythonPath".to_string(),
                    serde_json::Value::String(python_path),
                );
            }
            python.insert("analysis".to_string(), analysis);
            serde_json::Value::Object(python)
        }
        Some("python.analysis") => analysis,
        _ => serde_json::Value::Object(serde_json::Map::new()),
    }
}

const TYPESCRIPT_LSP: LspIdentity = LspIdentity {
    binary: "typescript-language-server",
    args: &["--stdio"],
    // typescript-language-server behaves fine with no initializationOptions (it discovers a
    // real, project-local `node_modules/typescript` itself) - see this crate's real, live-tested
    // TypeScript integration test for confirmation this isn't just an assumption.
    initialization_options: no_initialization_options,
    workspace_configuration: lsp_core::default_workspace_configuration,
    dynamic_args: no_dynamic_args,
    companion: None,
};

const PYRIGHT_LSP: LspIdentity = LspIdentity {
    binary: "pyright-langserver",
    args: &["--stdio"],
    initialization_options: pyright_initialization_options,
    workspace_configuration: pyright_workspace_configuration,
    dynamic_args: no_dynamic_args,
    companion: None,
};

const RUST_ANALYZER_LSP: LspIdentity = LspIdentity {
    binary: "rust-analyzer",
    args: &[],
    initialization_options: no_initialization_options,
    workspace_configuration: lsp_core::default_workspace_configuration,
    dynamic_args: no_dynamic_args,
    companion: None,
};

/// Vue's real companion: the *same* `typescript-language-server` binary this registry already
/// spawns for `.ts`/`.tsx`/`.js`/`.jsx`, but under its own distinct [`CompanionServer::client_key`]
/// and carrying the real `@vue/typescript-plugin` - see [`CompanionServer`]'s own field docs, and
/// this module's top-level docs for why a plain LSP server (rather than a bespoke raw-`tsserver`
/// connection) is the correct, reachable integration point for a generic LSP client like this app.
const VUE_TYPESCRIPT_COMPANION: CompanionServer = CompanionServer {
    client_key: "typescript-language-server (vue)",
    binary: "typescript-language-server",
    args: &["--stdio"],
    language_id: "vue",
    initialization_options: vue_companion_initialization_options,
    relay_request_method: "tsserver/request",
    relay_response_method: "tsserver/response",
    relay_command: "typescript.tsserverRequest",
    fallback_methods: VUE_FALLBACK_METHODS,
};

/// The three real requests a hybrid-mode `vue-language-server` genuinely does not answer for the
/// TypeScript inside a `.vue` file - measured by hand against the real installed pair, on a real
/// `.vue` fixture with a `<script setup lang="ts">` block, one method at a time:
///
/// | request                     | `vue-language-server` | `typescript-language-server` + plugin |
/// |-----------------------------|-----------------------|---------------------------------------|
/// | `textDocument/hover`        | `null`                | ``const bad: number``                 |
/// | `textDocument/definition`   | `[]`                  | one real `Location`                   |
/// | `textDocument/completion`   | `items: []`           | `["alpha", "beta"]`                   |
///
/// Note the three different *shapes* of "nothing here" in that middle column - only hover's is a
/// wire `null`. That's why `crate::lsp::client::lsp_result_is_empty`, not a plain null check, is
/// what decides when to actually replay one of these against the companion.
///
/// Deliberately not "every method": a request the primary answers correctly must keep going to the
/// primary alone, so the companion can never quietly substitute a different answer for a real one
/// (`crate::lsp::client::lsp_connection_facade_tests::a_request_outside_the_fallback_list_never_consults_the_companion`).
const VUE_FALLBACK_METHODS: &[&str] = &[
    HoverRequest::METHOD,
    GotoDefinition::METHOD,
    Completion::METHOD,
];

const VUE_LSP: LspIdentity = LspIdentity {
    binary: "vue-language-server",
    args: &["--stdio"],
    // No `initializationOptions` needed: the one real setting this server requires (`tsdk`) is
    // supplied on the command line by [`vue_dynamic_args`] instead, which is where the real,
    // repo-root-dependent resolution lives.
    initialization_options: no_initialization_options,
    workspace_configuration: lsp_core::default_workspace_configuration,
    dynamic_args: vue_dynamic_args,
    companion: Some(VUE_TYPESCRIPT_COMPANION),
};

/// The one real table every extension-keyed lookup in this crate now reads from - see this
/// module's top-level docs. Order matters only for [`tests::no_duplicate_extensions`]'s own
/// diagnostics; lookups are by value, not position.
pub const EXTENSIONS: &[ExtensionEntry] = &[
    ExtensionEntry {
        extension: "rs",
        display_name: "Rust",
        lsp_language_id: "rust",
        chip_label: "rs",
        chip_colors: theme::lang::RS,
        lsp: Some(RUST_ANALYZER_LSP),
        settings_row: Some(SettingsLspRow {
            binary: "rust-analyzer",
            note: "starts when a .rs file opens",
            install_url: "https://rust-analyzer.github.io/book/rust_analyzer_binary.html",
        }),
        highlighter: Some(crate::code_surface::code_view::highlight_rust),
    },
    ExtensionEntry {
        extension: "toml",
        display_name: "TOML",
        lsp_language_id: "toml",
        chip_label: "to",
        chip_colors: theme::lang::TOML,
        lsp: None,
        settings_row: None,
        // Real `tree-sitter-toml-ng` grammar, GitHub issue #32.
        highlighter: Some(crate::code_surface::code_view::highlight_toml),
    },
    ExtensionEntry {
        extension: "md",
        display_name: "Markdown",
        lsp_language_id: "markdown",
        chip_label: "md",
        chip_colors: theme::lang::MD,
        lsp: None,
        settings_row: None,
        // Real `tree-sitter-md` block+inline grammar, GitHub issue #104.
        highlighter: Some(crate::code_surface::code_view::highlight_markdown),
    },
    ExtensionEntry {
        extension: "sql",
        display_name: "SQL",
        lsp_language_id: "sql",
        chip_label: "sq",
        chip_colors: theme::lang::SQL,
        lsp: None,
        settings_row: None,
        highlighter: None,
    },
    ExtensionEntry {
        extension: "ts",
        display_name: "TypeScript",
        lsp_language_id: "typescript",
        chip_label: "ts",
        chip_colors: theme::lang::TS,
        lsp: Some(TYPESCRIPT_LSP),
        settings_row: Some(SettingsLspRow {
            binary: "typescript-language-server",
            note: "starts when a .ts file opens",
            install_url: "https://github.com/typescript-language-server/typescript-language-server",
        }),
        highlighter: Some(crate::code_surface::code_view::highlight_ts),
    },
    ExtensionEntry {
        extension: "tsx",
        display_name: "TypeScript (TSX)",
        lsp_language_id: "typescriptreact",
        chip_label: "ts",
        chip_colors: theme::lang::TS,
        lsp: Some(TYPESCRIPT_LSP),
        settings_row: None,
        highlighter: Some(crate::code_surface::code_view::highlight_tsx),
    },
    ExtensionEntry {
        extension: "js",
        display_name: "JavaScript",
        lsp_language_id: "javascript",
        chip_label: "ts",
        chip_colors: theme::lang::TS,
        lsp: Some(TYPESCRIPT_LSP),
        settings_row: None,
        // `.js` deliberately reuses the plain TypeScript grammar (a real syntactic superset of
        // JavaScript) rather than `highlight_tsx` - see `crate::code_surface::code_view::highlight_typescript`'s
        // own docs.
        highlighter: Some(crate::code_surface::code_view::highlight_ts),
    },
    ExtensionEntry {
        extension: "jsx",
        display_name: "JavaScript (JSX)",
        lsp_language_id: "javascriptreact",
        chip_label: "ts",
        chip_colors: theme::lang::TS,
        lsp: Some(TYPESCRIPT_LSP),
        settings_row: None,
        highlighter: Some(crate::code_surface::code_view::highlight_tsx),
    },
    ExtensionEntry {
        extension: "vue",
        display_name: "Vue",
        lsp_language_id: "vue",
        chip_label: "vue",
        chip_colors: theme::lang::VUE,
        // Real, and genuinely two coordinated processes - see this module's top-level docs and
        // [`CompanionServer`]. Deferred through Revision R8 for a real, reproduced reason; now
        // built rather than still deferred.
        lsp: Some(VUE_LSP),
        settings_row: Some(SettingsLspRow {
            binary: "vue-language-server",
            // Honest about the real shape of what starts: a `.vue` file genuinely brings up two
            // real processes, not one, and the second one carries the TypeScript half of the
            // analysis (see this module's top-level docs for the live-verified split).
            note: "starts when a .vue file opens, with a TypeScript companion",
            install_url: "https://github.com/vuejs/language-tools",
        }),
        // No `.vue` `tree-sitter` grammar dependency exists in this workspace - unrelated to,
        // and independent of, why `lsp` above is also `None`.
        highlighter: None,
    },
    ExtensionEntry {
        extension: "py",
        display_name: "Python",
        lsp_language_id: "python",
        chip_label: "py",
        chip_colors: theme::lang::PY,
        lsp: Some(PYRIGHT_LSP),
        settings_row: Some(SettingsLspRow {
            binary: "pyright-langserver",
            note: "starts when a .py file opens",
            install_url: "https://github.com/microsoft/pyright/blob/main/docs/installation.md",
        }),
        highlighter: Some(crate::code_surface::code_view::highlight_python),
    },
    ExtensionEntry {
        extension: "go",
        display_name: "Go",
        lsp_language_id: "go",
        chip_label: "go",
        chip_colors: theme::lang::GO,
        // Out of this phase's real scope (the user's ask named TypeScript/Vue/Python) - see this
        // module's top-level docs.
        lsp: None,
        settings_row: Some(SettingsLspRow {
            binary: "gopls",
            note: "installs when the first .go file opens",
            install_url: "https://go.dev/gopls/",
        }),
        // Real `tree-sitter-go` grammar, GitHub issue #32 - independent of, and unrelated to,
        // `lsp` still being `None` above.
        highlighter: Some(crate::code_surface::code_view::highlight_go),
    },
    ExtensionEntry {
        extension: "json",
        display_name: "JSON",
        lsp_language_id: "json",
        chip_label: "jsn",
        chip_colors: theme::lang::JSON,
        lsp: None,
        settings_row: None,
        highlighter: Some(crate::code_surface::code_view::highlight_json),
    },
    ExtensionEntry {
        extension: "yaml",
        display_name: "YAML",
        lsp_language_id: "yaml",
        chip_label: "yml",
        chip_colors: theme::lang::YAML,
        lsp: None,
        settings_row: None,
        highlighter: Some(crate::code_surface::code_view::highlight_yaml),
    },
    ExtensionEntry {
        extension: "yml",
        display_name: "YAML",
        lsp_language_id: "yaml",
        chip_label: "yml",
        chip_colors: theme::lang::YAML,
        lsp: None,
        settings_row: None,
        // `.yml` and `.yaml` are the same real language under two conventional extensions - no
        // grammar or chip distinguishes them, matching how `.ts`/`.js` already share plumbing
        // where the underlying grammar genuinely is the same.
        highlighter: Some(crate::code_surface::code_view::highlight_yaml),
    },
    ExtensionEntry {
        extension: "c",
        display_name: "C",
        lsp_language_id: "c",
        chip_label: "c",
        chip_colors: theme::lang::C,
        lsp: None,
        settings_row: None,
        highlighter: Some(crate::code_surface::code_view::highlight_c),
    },
    ExtensionEntry {
        extension: "h",
        display_name: "C Header",
        lsp_language_id: "c",
        chip_label: "c",
        chip_colors: theme::lang::C,
        lsp: None,
        settings_row: None,
        // A header has no real syntax this grammar treats differently from a `.c` file - see
        // `crate::code_surface::code_view::highlight_c`'s own docs.
        highlighter: Some(crate::code_surface::code_view::highlight_c),
    },
    // GitHub issue #154. No `lsp` for either: this module's own scope docs above reserve a real
    // server for a language an explicit user ask named, and issue #154's text asks for syntax
    // support only - the same treatment TOML/Markdown/JSON/YAML/C already get.
    ExtensionEntry {
        extension: "html",
        display_name: "HTML",
        lsp_language_id: "html",
        chip_label: "htm",
        chip_colors: theme::lang::HTML,
        lsp: None,
        settings_row: None,
        highlighter: Some(crate::code_surface::code_view::highlight_html),
    },
    ExtensionEntry {
        extension: "htm",
        display_name: "HTML",
        lsp_language_id: "html",
        chip_label: "htm",
        chip_colors: theme::lang::HTML,
        lsp: None,
        settings_row: None,
        // The same real grammar - `.htm` is a legacy spelling of the identical language, matching
        // how `.yml`/`.yaml` and `.c`/`.h` already share one grammar here.
        highlighter: Some(crate::code_surface::code_view::highlight_html),
    },
    ExtensionEntry {
        extension: "css",
        display_name: "CSS",
        lsp_language_id: "css",
        chip_label: "css",
        chip_colors: theme::lang::CSS,
        lsp: None,
        settings_row: None,
        highlighter: Some(crate::code_surface::code_view::highlight_css),
    },
];

/// The [`EXTENSIONS`] key a fenced code block's own info-string language name maps to - a fence
/// says ` ```rust `, this registry is keyed by the file extension `"rs"`, and those are genuinely
/// two different vocabularies, so this is a real, small alias table rather than a guess.
///
/// The **one** such table in this crate, deliberately. It started life private to
/// `crate::code_surface::markdown_preview` (colouring a *rendered* preview's code cards) and was
/// lifted here by GitHub issue #154, which needed the identical mapping for a second real caller:
/// `crate::code_surface::code_view`'s tree-sitter injection callback, which resolves a fence's
/// info string to a real grammar so the fence's content is highlighted in **source** view too.
/// Two copies would have meant ` ```py ` colouring in one view and not the other, with nothing to
/// catch it; `tests::every_fence_alias_names_a_real_registry_extension` and
/// `crate::code_surface::code_view`'s own
/// `every_registry_extension_with_a_highlighter_is_reachable_as_a_fence_language` are the real
/// drift guards over that.
///
/// `None` for an unrecognized tag (` ```zig `, ` ```console `, or a fence with no tag at all) is
/// the honest answer both callers already handle by rendering that block as plain text.
pub fn extension_for_fence_language(language: &str) -> Option<&'static str> {
    match language.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some("rs"),
        "python" | "py" => Some("py"),
        "typescript" | "ts" => Some("ts"),
        "tsx" => Some("tsx"),
        "javascript" | "js" => Some("js"),
        "jsx" => Some("jsx"),
        "go" | "golang" => Some("go"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "c" => Some("c"),
        "h" => Some("h"),
        // GitHub issue #154's own additions. `"markdown"`/`"md"` is real and not circular: a
        // markdown fence tagged `markdown` reparses strictly less text than its host document
        // (see `code_view::injection_config`'s own docs on why that terminates).
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "markdown" | "md" => Some("md"),
        _ => None,
    }
}

/// The real, canonical source for `crate::settings::state::LSP_LANGUAGES` - every [`ExtensionEntry`]
/// with a real [`ExtensionEntry::settings_row`], in [`EXTENSIONS`]' own order.
pub fn settings_lsp_entries() -> impl Iterator<Item = &'static ExtensionEntry> {
    EXTENSIONS
        .iter()
        .filter(|entry| entry.settings_row.is_some())
}

/// Looks up `extension` (case-insensitive) in [`EXTENSIONS`] - the one real place every
/// extension-keyed lookup in this crate now goes through. `O(EXTENSIONS.len())`, a fixed, tiny
/// (11-entry) linear scan - cheap enough to call on every render (file-tree rows, the File view's
/// status bar) without memoizing, matching how the four tables this replaces were each already
/// called per-render.
pub fn entry_for_extension(extension: Option<&str>) -> Option<&'static ExtensionEntry> {
    let extension = extension?;
    EXTENSIONS
        .iter()
        .find(|entry| entry.extension.eq_ignore_ascii_case(extension))
}

/// The File view status bar's language label - `"Plain Text"` for any extension not in
/// [`EXTENSIONS`] (including no extension at all), never a fabricated specific-sounding label.
pub fn display_name_for_extension(extension: Option<&str>) -> &'static str {
    entry_for_extension(extension)
        .map(|entry| entry.display_name)
        .unwrap_or("Plain Text")
}

/// The file-tree/palette/code-tab chip for `extension` - `(label, fg, bg)`, falling back to
/// `theme::lang::UNKNOWN` under the neutral `"."` label for anything not in [`EXTENSIONS`], so
/// every file row still gets *some* chip.
pub fn chip_for_extension(extension: Option<&str>) -> (&'static str, Rgba, Rgba) {
    match entry_for_extension(extension) {
        Some(entry) => (
            entry.chip_label,
            entry.chip_colors.0.into(),
            entry.chip_colors.1.into(),
        ),
        None => (
            ".",
            theme::lang::UNKNOWN.0.into(),
            theme::lang::UNKNOWN.1.into(),
        ),
    }
}

/// The real `textDocument/didOpen` `language_id` for `extension`, if this crate would spawn an
/// LSP client for it at all (`None` for an extension with no [`ExtensionEntry::lsp`], e.g.
/// `.go`, or one absent from [`EXTENSIONS`] entirely) - the replacement for the old
/// `is_rust` boolean gate in `crate::code_surface`: "is there an `lsp_language_id` for this
/// extension" now answers "should this app try to talk to a language server for this file" for
/// every supported language, not just Rust.
pub fn lsp_language_id_for_extension(extension: Option<&str>) -> Option<&'static str> {
    let entry = entry_for_extension(extension)?;
    entry.lsp.is_some().then_some(entry.lsp_language_id)
}

/// The real server binary this extension's LSP client is keyed by (see
/// `crate::root::AdeApp::lsp_clients`'s own docs for why the map key was widened to
/// `(PathBuf, &'static str)` - this is that second half) - `None` for an extension with no real
/// server spawned for it.
pub fn lsp_binary_for_extension(extension: Option<&str>) -> Option<&'static str> {
    entry_for_extension(extension).and_then(|entry| entry.lsp.map(|lsp| lsp.binary))
}

/// Builds a real, owned [`ServerSpawnConfig`] for `extension` under the real repository at
/// `repo_root`. Calls [`LspIdentity::initialization_options`]/[`LspIdentity::dynamic_args`] fresh
/// each time (see those fields' own docs on why that's correctly *not* memoized: both can depend
/// on real, possibly-changed PATH/filesystem state at the moment of a real spawn).
///
/// The three-way return distinguishes two genuinely different "no server" answers that used to be
/// collapsed into one:
///
/// - `Ok(None)` - a **static** fact: this extension has no [`ExtensionEntry::lsp`] entry at all
///   (TOML, Go, anything unrecognized). Nothing failed; there is simply nothing to spawn.
/// - `Err(reason)` - a **dynamic** failure: there is a real LSP entry, but a real, checked
///   prerequisite for it is missing (see [`vue_dynamic_args`]). The caller surfaces `reason`
///   honestly rather than spawning a process already known not to work.
/// - `Ok(Some(config))` - a real, spawnable configuration.
pub fn server_spawn_config(
    repo_root: &Path,
    extension: Option<&str>,
) -> Result<Option<ServerSpawnConfig>, String> {
    let Some(lsp) = entry_for_extension(extension).and_then(|entry| entry.lsp) else {
        return Ok(None);
    };
    let mut args: Vec<String> = lsp.args.iter().map(|arg| arg.to_string()).collect();
    args.extend((lsp.dynamic_args)(repo_root)?);
    Ok(Some(ServerSpawnConfig {
        name: lsp.binary,
        binary: lsp.binary,
        args,
        initialization_options: (lsp.initialization_options)(),
        workspace_configuration: lsp.workspace_configuration,
        // A primary with a companion is the only kind of server this app has a real handler for a
        // custom notification from, and the only method it handles is that companion's own
        // declared relay request - so that is exactly, and only, what it subscribes to. Every
        // other server subscribes to nothing and pays nothing (see
        // [`lsp_core::ServerSpawnConfig::custom_notification_methods`]).
        custom_notification_methods: lsp
            .companion
            .map(|companion| vec![companion.relay_request_method])
            .unwrap_or_default(),
    }))
}

/// The [`CompanionServer`] this extension's primary needs alongside it, if any - `None` for every
/// single-process language, and for an extension with no LSP entry at all. Mirrors
/// [`lsp_binary_for_extension`]'s shape: a cheap, static registry lookup safe to call on the
/// render path.
pub fn companion_for_extension(extension: Option<&str>) -> Option<CompanionServer> {
    entry_for_extension(extension).and_then(|entry| entry.lsp.and_then(|lsp| lsp.companion))
}

/// The [`CompanionServer`] belonging to whichever registry entry spawns `binary` as its *primary*
/// server - the real, honest way for `crate::lsp::client`'s poll loop to ask "is this specific
/// client the kind that can send a relay notification, and if so where do its relays go?" without
/// naming any particular server, and without blanket-handling a method for every client.
pub fn companion_for_primary_binary(binary: &str) -> Option<CompanionServer> {
    EXTENSIONS
        .iter()
        .filter_map(|entry| entry.lsp)
        .find(|lsp| lsp.binary == binary)
        .and_then(|lsp| lsp.companion)
}

/// The `initializationOptions` fragment that turns a server's auto-import completions off, for a
/// server where that is known - by direct verification, not by reading a settings reference - to
/// actually work. `None` for every other server, so nothing is sent on a guess.
///
/// Backs `crate::settings::store::EditorSettings::suggest_auto_imports`. Only
/// `typescript-language-server` is here, and each of these was checked against a live server in a
/// real project rather than assumed:
///
/// - `includeCompletionsForModuleExports: false` **works**: at the reported caret it takes the
///   response from 1029 items with 7 Node auto-import candidates down to 1019 with none.
/// - `includePackageJsonAutoImports: "off"` does **not**, despite its name: byte-identical
///   response, all 7 candidates still there. Not sent.
/// - `pyright-langserver` ignores its own `python.analysis.autoImportCompletions` here entirely -
///   it reads that over `workspace/configuration` instead, where the same value really does work
///   (48 items down to 7). Reaching that path needs a `WorkspaceConfigFn` that can see settings,
///   which is a real change to `lsp_core`'s own API and is deliberately not in this commit.
/// - `rust-analyzer` is unverified: its probe needs a fully primed index and did not finish here.
///   Nothing is sent for it rather than a plausible-looking key.
pub fn auto_import_suppression_options(server_name: &str) -> Option<serde_json::Value> {
    // Both the standalone TypeScript server and the one Vue runs as a companion, which is the
    // same binary answering for the `<script>` half of an SFC.
    if server_name.starts_with("typescript-language-server") {
        return Some(serde_json::json!({
            "preferences": { "includeCompletionsForModuleExports": false },
        }));
    }
    None
}

/// Deep-merges a user's own `settings.toml` `initializationOptions` **over** whatever this
/// registry built for that server, and hands back what should actually be sent.
///
/// Merged rather than substituted, because the registry's own options are load-bearing and a user
/// setting one unrelated key must not silently drop them: Pyright's real resolved `pythonPath`,
/// `vue-language-server`'s real resolved `--tsdk`. Objects merge key by key, recursively; anything
/// else (a string, a number, an **array**) the user supplied replaces what was there, since a
/// half-overridden array is nobody's intent.
///
/// Exists because there is no other way to configure a server here. `initializationOptions` is
/// where a real server takes its own behavioural settings, it is emphatically *not* something a
/// `tsconfig.json` can reach (checked directly against a real `typescript-language-server`, which
/// reads them from `initializationOptions.preferences` at startup and asks the client for nothing
/// but `formattingOptions` afterwards), and this app previously sent `None` for every server that
/// had no built-in options of its own.
pub fn merge_initialization_options(
    built_in: Option<serde_json::Value>,
    user: Option<&toml::Value>,
) -> Option<serde_json::Value> {
    merge_initialization_options_json(
        built_in,
        user.and_then(|user| serde_json::to_value(user).ok()),
    )
}

/// [`merge_initialization_options`] over an overlay this app built itself rather than one read out
/// of a TOML file - the same merge rule, shared so a toggle and a `settings.toml` entry can be
/// layered one after the other.
pub fn merge_initialization_options_json(
    built_in: Option<serde_json::Value>,
    overlay: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    match (built_in, overlay) {
        (base, None) => base,
        (None, Some(overlay)) => Some(overlay),
        (Some(base), Some(overlay)) => Some(merge_json(base, overlay)),
    }
}

/// [`merge_initialization_options`]'s recursive half - see its own docs for the merge rule.
fn merge_json(base: serde_json::Value, user: serde_json::Value) -> serde_json::Value {
    match (base, user) {
        (serde_json::Value::Object(mut base), serde_json::Value::Object(user)) => {
            for (key, value) in user {
                let merged = match base.remove(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            serde_json::Value::Object(base)
        }
        (_, user) => user,
    }
}

/// Builds a real, owned [`ServerSpawnConfig`] for `companion`. `Err(reason)` when its own real,
/// checked prerequisite is genuinely missing (see [`CompanionServer::initialization_options`]) -
/// which the caller surfaces as that companion's own `Failed` state without preventing the primary
/// from still starting, since a primary alone is a real, if reduced, working experience.
pub fn companion_spawn_config(companion: &CompanionServer) -> Result<ServerSpawnConfig, String> {
    Ok(ServerSpawnConfig {
        // The distinct `client_key`, not the plain `binary` name - so every log line, error
        // message and status string this companion produces says which of the two
        // `typescript-language-server` processes a repo root might have running it actually is.
        name: companion.client_key,
        binary: companion.binary,
        args: companion.args.iter().map(|arg| arg.to_string()).collect(),
        initialization_options: (companion.initialization_options)()?,
        workspace_configuration: lsp_core::default_workspace_configuration,
        // A companion never sends a relay request of its own - it only ever answers one - so it
        // has no custom notification for this app to handle.
        custom_notification_methods: Vec::new(),
    })
}

/// Real file extension -> language-family mapping for `path`, used by
/// `crate::root::AdeApp::lsp_client_for_path` and friends to go from "a file" to "the real
/// registry entry that would spawn/talk to a server for it" in one place.
pub fn entry_for_path(path: &Path) -> Option<&'static ExtensionEntry> {
    entry_for_extension(path.extension().and_then(|ext| ext.to_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicate_extensions() {
        let mut seen = std::collections::HashSet::new();
        for entry in EXTENSIONS {
            assert!(
                seen.insert(entry.extension),
                "extension {:?} appears more than once in EXTENSIONS - the one real source of \
                 truth must have exactly one entry per extension",
                entry.extension
            );
        }
    }

    #[test]
    fn every_extension_is_lowercase_with_no_leading_dot() {
        for entry in EXTENSIONS {
            assert_eq!(
                entry.extension,
                entry.extension.to_ascii_lowercase(),
                "extension {:?} should already be lowercase - lookups lowercase the query, not \
                 the table",
                entry.extension
            );
            assert!(
                !entry.extension.starts_with('.'),
                "extension {:?} should not carry a leading dot",
                entry.extension
            );
        }
    }

    #[test]
    fn entry_for_extension_is_case_insensitive() {
        let upper = entry_for_extension(Some("RS")).expect("Cargo.TOML-style uppercase match");
        assert_eq!(upper.extension, "rs");
    }

    #[test]
    fn entry_for_extension_is_none_for_an_unknown_extension() {
        assert!(entry_for_extension(Some("xyz")).is_none());
        assert!(entry_for_extension(None).is_none());
    }

    #[test]
    fn display_name_falls_back_to_plain_text() {
        assert_eq!(display_name_for_extension(Some("rs")), "Rust");
        assert_eq!(display_name_for_extension(Some("ts")), "TypeScript");
        assert_eq!(display_name_for_extension(Some("py")), "Python");
        assert_eq!(display_name_for_extension(Some("vue")), "Vue");
        assert_eq!(display_name_for_extension(Some("xyz")), "Plain Text");
        assert_eq!(display_name_for_extension(None), "Plain Text");
    }

    #[test]
    fn chip_for_extension_falls_back_to_the_neutral_unknown_chip() {
        let (label, fg, bg) = chip_for_extension(Some("rs"));
        assert_eq!(label, "rs");
        assert_eq!(
            (fg, bg),
            (theme::lang::RS.0.into(), theme::lang::RS.1.into())
        );

        let (label, fg, bg) = chip_for_extension(Some("xyz"));
        assert_eq!(label, ".");
        assert_eq!(
            (fg, bg),
            (theme::lang::UNKNOWN.0.into(), theme::lang::UNKNOWN.1.into())
        );
    }

    #[test]
    fn typescript_family_extensions_share_one_binary_but_have_distinct_language_ids() {
        let ts = entry_for_extension(Some("ts")).expect("ts entry");
        let tsx = entry_for_extension(Some("tsx")).expect("tsx entry");
        let js = entry_for_extension(Some("js")).expect("js entry");
        let jsx = entry_for_extension(Some("jsx")).expect("jsx entry");

        for entry in [ts, tsx, js, jsx] {
            assert_eq!(
                entry.lsp.expect("real lsp identity").binary,
                "typescript-language-server"
            );
        }
        let ids: std::collections::HashSet<_> = [ts, tsx, js, jsx]
            .iter()
            .map(|e| e.lsp_language_id)
            .collect();
        assert_eq!(
            ids.len(),
            4,
            "each real extension needs its own real language_id, not one shared constant"
        );
    }

    /// A scratch repo root with a real, project-local classic-layout TypeScript's own
    /// `lib/typescript.js` present - the exact real file [`vue_dynamic_args`] existence-checks.
    /// A genuine `npm install typescript` would produce a ~9 MB one; this writes a tiny stand-in
    /// because what's under test here is only this crate's own resolution/refusal logic, which
    /// looks at nothing but the path. The real, live end-to-end Vue test in `crate::lsp::client`
    /// uses a genuine `npm install typescript` instead, since *that* one hands the path to a real
    /// server that really does load it.
    fn scratch_root_with_local_typescript() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let lib = dir
            .path()
            .join("node_modules")
            .join("typescript")
            .join("lib");
        std::fs::create_dir_all(&lib).expect("mkdir node_modules/typescript/lib");
        std::fs::write(lib.join("typescript.js"), "// stand-in\n").expect("write typescript.js");
        dir
    }

    #[test]
    fn go_is_detected_but_never_spawns_a_real_client() {
        let root = tempfile::TempDir::new().expect("tempdir");
        assert!(lsp_language_id_for_extension(Some("go")).is_none());
        assert!(lsp_binary_for_extension(Some("go")).is_none());
        assert!(
            matches!(server_spawn_config(root.path(), Some("go")), Ok(None)),
            "an extension with no LSP entry at all is a static `Ok(None)`, not a failure"
        );
        assert_eq!(chip_for_extension(Some("go")).0, "go");
    }

    #[test]
    fn rust_typescript_and_python_all_produce_a_real_spawn_config() {
        let root = tempfile::TempDir::new().expect("tempdir");
        for ext in ["rs", "ts", "py"] {
            let config = server_spawn_config(root.path(), Some(ext))
                .unwrap_or_else(|err| panic!("{ext} should not fail a prerequisite check: {err}"))
                .unwrap_or_else(|| panic!("{ext} should have a real spawn config"));
            assert!(!config.binary.is_empty());
            assert_eq!(config.name, config.binary);
        }
    }

    /// The real regression guard for this revision's generalization: the three already-working
    /// languages' actual spawn command lines must be **byte-for-byte** what they were before
    /// [`LspIdentity::dynamic_args`]/[`LspIdentity::companion`] existed. Pins the exact real
    /// `args` lists (not just "unchanged in spirit"), and proves both new fields are genuinely
    /// inert for them - `dynamic_args` an unconditional empty vec, `companion` a `None` - so no
    /// second process can ever appear for a `.rs`/`.ts`/`.py` file.
    #[test]
    fn the_three_pre_existing_languages_spawn_exactly_what_they_did_before() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let expected: [(&str, &str, &[&str]); 3] = [
            ("rs", "rust-analyzer", &[]),
            ("ts", "typescript-language-server", &["--stdio"]),
            ("py", "pyright-langserver", &["--stdio"]),
        ];
        for (ext, binary, args) in expected {
            let entry = entry_for_extension(Some(ext)).expect("a real registry entry");
            let lsp = entry.lsp.expect("a real lsp identity");
            assert_eq!(
                (lsp.dynamic_args)(root.path()),
                Ok(Vec::new()),
                "{ext}'s dynamic_args must be a genuine no-op, adding nothing to its real \
                 command line"
            );
            assert!(
                lsp.companion.is_none(),
                "{ext} is a real single-process language and must never gain a companion"
            );
            assert!(
                companion_for_extension(Some(ext)).is_none(),
                "{ext} must resolve to no companion through the public lookup too"
            );

            let config = server_spawn_config(root.path(), Some(ext))
                .expect("no prerequisite check applies")
                .expect("a real spawn config");
            assert_eq!(config.binary, binary);
            assert_eq!(config.name, binary);
            assert_eq!(
                config.args,
                args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
                "{ext}'s real spawn args must be exactly what they were before dynamic_args \
                 existed"
            );
        }
    }

    /// Vue's real `--tsdk` resolution, against a real repo root that genuinely has a
    /// project-local `node_modules/typescript/lib/typescript.js`.
    #[test]
    fn vue_appends_a_real_tsdk_argument_pointing_at_the_projects_own_typescript() {
        let root = scratch_root_with_local_typescript();
        let config = server_spawn_config(root.path(), Some("vue"))
            .expect("a real project-local typescript is present, so this must not fail")
            .expect("vue has a real lsp entry");
        assert_eq!(config.binary, "vue-language-server");
        let expected_tsdk = root
            .path()
            .join("node_modules")
            .join("typescript")
            .join("lib");
        assert_eq!(
            config.args,
            vec![
                "--stdio".to_string(),
                format!("--tsdk={}", expected_tsdk.display()),
            ],
            "the static args come first, then the real, dynamically-resolved --tsdk"
        );
    }

    /// The honest-failure half: with no real project-local TypeScript there is genuinely nothing
    /// valid to point `--tsdk` at, so this must be a real `Err` naming the actual missing
    /// prerequisite - never a fabricated path, and never a silent `Ok` that would spawn a process
    /// already known to crash.
    #[test]
    fn vue_refuses_to_spawn_when_the_project_has_no_local_typescript() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let error = server_spawn_config(root.path(), Some("vue"))
            .expect_err("no project-local typescript means a real, refused spawn");
        assert!(
            error.contains("npm install typescript"),
            "the message should name the real, actionable prerequisite, got: {error}"
        );
        assert!(
            error.contains(&root.path().display().to_string()),
            "the message should name the real root it actually looked under, got: {error}"
        );
    }

    /// A `node_modules/typescript/lib` directory that exists but has no real `typescript.js` in it
    /// (the shape a restructured TypeScript 7.x install has) must still be refused - the
    /// existence check is on that specific file precisely because the directory alone doesn't
    /// distinguish the layout that works from the one that reproduces the crash.
    #[test]
    fn vue_refuses_a_typescript_lib_directory_with_no_real_typescript_js_in_it() {
        let root = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(
            root.path()
                .join("node_modules")
                .join("typescript")
                .join("lib"),
        )
        .expect("mkdir");
        assert!(
            server_spawn_config(root.path(), Some("vue")).is_err(),
            "a directory with no real lib/typescript.js is not a usable --tsdk target"
        );
    }

    /// Vue's real companion identity - pinned exactly, since every one of these values is a real
    /// wire-level requirement verified against both live processes, not a naming preference.
    #[test]
    fn vue_carries_a_real_distinctly_keyed_typescript_companion() {
        let companion = companion_for_extension(Some("vue")).expect("vue has a real companion");
        assert_eq!(companion.binary, "typescript-language-server");
        assert_eq!(companion.args, &["--stdio"]);
        assert_eq!(companion.language_id, "vue");
        assert_eq!(companion.relay_request_method, "tsserver/request");
        assert_eq!(companion.relay_response_method, "tsserver/response");
        assert_eq!(companion.relay_command, "typescript.tsserverRequest");
        assert_eq!(
            companion.fallback_methods,
            &[
                "textDocument/hover",
                "textDocument/definition",
                "textDocument/completion"
            ],
            "all three of these were measured, by hand, to come back genuinely empty from the \
             real primary inside a .vue <script setup lang=\"ts\"> block while the real companion \
             answers each of them - see VUE_FALLBACK_METHODS' own table"
        );
        assert_ne!(
            companion.client_key, companion.binary,
            "the companion's client key must differ from the plain binary name, so a \
             Vue-flavored typescript-language-server (which carries an extra real plugin) can \
             never be silently reused as - or collide with - the plain one a .ts file spawns"
        );
        assert_eq!(
            companion_for_primary_binary("vue-language-server").map(|c| c.client_key),
            Some(companion.client_key),
            "the binary-keyed lookup the forwarding dispatch uses must find the same companion"
        );
        for binary in [
            "rust-analyzer",
            "typescript-language-server",
            "pyright-langserver",
        ] {
            assert!(
                companion_for_primary_binary(binary).is_none(),
                "{binary} is a real single-process primary and must not resolve a companion"
            );
        }
    }

    /// The companion's own real prerequisite resolution, run against the actually-installed
    /// toolchain. Both outcomes are honest and neither is a test failure: if a real
    /// `vue-language-server` (with its real nested `@vue/typescript-plugin`) is installed, this
    /// must produce a real plugin entry pointing at a directory that genuinely exists on disk; if
    /// it isn't installed, this must be a real `Err` explaining which piece is missing rather
    /// than a fabricated success.
    #[test]
    fn the_vue_companions_plugin_resolution_is_real_either_way() {
        match vue_companion_initialization_options() {
            Ok(options) => {
                let options =
                    options.expect("the companion always needs real initializationOptions");
                let plugin = &options["plugins"][0];
                assert_eq!(plugin["name"], "@vue/typescript-plugin");
                assert_eq!(plugin["languages"], serde_json::json!(["vue"]));
                let location = plugin["location"]
                    .as_str()
                    .expect("a real, resolved location path");
                let plugin_dir = Path::new(location)
                    .join("node_modules")
                    .join("@vue")
                    .join("typescript-plugin");
                assert!(
                    plugin_dir.is_dir(),
                    "the resolved location must be a real probe directory that genuinely \
                     contains the plugin, since tsserver resolves it by Node module resolution \
                     from there - got {location}"
                );
            }
            Err(error) => {
                assert!(
                    error.contains("vue-language-server") || error.contains("typescript-plugin"),
                    "an honest failure should name the real missing piece, got: {error}"
                );
            }
        }
    }

    #[test]
    fn pyright_gets_real_non_null_initialization_options() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let config = server_spawn_config(root.path(), Some("py"))
            .expect("python has no dynamic prerequisite")
            .expect("python spawn config");
        let options = config
            .initialization_options
            .expect("Pyright should get real, non-None initializationOptions");
        assert!(options.get("python").is_some());
    }

    #[test]
    fn pyright_workspace_configuration_never_replies_null_for_a_known_section() {
        let python = pyright_workspace_configuration(Some("python"));
        assert!(!python.is_null());
        assert!(python.get("analysis").is_some());

        let analysis = pyright_workspace_configuration(Some("python.analysis"));
        assert!(!analysis.is_null());
        assert!(analysis.get("autoSearchPaths").is_some());
    }

    #[test]
    fn typescript_and_rust_use_the_shared_default_empty_object_workspace_configuration() {
        let ts = entry_for_extension(Some("ts")).expect("ts entry");
        let value = (ts.lsp.expect("lsp").workspace_configuration)(Some("typescript.inlayHints"));
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn entry_for_path_reads_a_real_extension_off_a_path() {
        let entry = entry_for_path(Path::new("src/main.rs")).expect("rs entry");
        assert_eq!(entry.extension, "rs");
        assert!(entry_for_path(Path::new("README")).is_none());
    }

    /// The real drift guard for finding 5's fix: every extension this crate genuinely has a real
    /// `tree-sitter` grammar for has a real [`ExtensionEntry::highlighter`] wired in this same
    /// registry - not a second, independent table `crate::code_surface::code_view::load_file` used to maintain
    /// on its own, invisible to this one. Pinning the exact real set (rather than just "some are
    /// Some") means a future extension added here with a real grammar but a forgotten
    /// `highlighter` wiring changes this set and fails this test, rather than silently rendering
    /// as plain text with nothing to catch the gap.
    #[test]
    fn every_extension_with_a_real_tree_sitter_grammar_has_a_highlighter_wired() {
        let mut with_highlighter: Vec<&str> = EXTENSIONS
            .iter()
            .filter(|entry| entry.highlighter.is_some())
            .map(|entry| entry.extension)
            .collect();
        with_highlighter.sort_unstable();
        assert_eq!(
            with_highlighter,
            vec![
                "c", "css", "go", "h", "htm", "html", "js", "json", "jsx", "md", "py", "rs",
                "toml", "ts", "tsx", "yaml", "yml"
            ],
            "this is the real, current set of extensions with a genuine tree-sitter grammar \
             dependency - a change here should be a deliberate decision, not a silent drift"
        );
    }

    /// The other half of the same guard: extensions with no real `tree-sitter` grammar dependency
    /// in this workspace (SQL never had one - GitHub issue #32's own stated remaining scope;
    /// Vue's lack of one is independent of whether it spawns a real server; Markdown gained a
    /// real one in GitHub issue #104) must not carry a stray, fabricated `highlighter`.
    #[test]
    fn extensions_with_no_real_grammar_have_no_highlighter_wired() {
        for ext in ["sql", "vue"] {
            let entry = entry_for_extension(Some(ext))
                .unwrap_or_else(|| panic!("{ext} should have a real registry entry"));
            assert!(
                entry.highlighter.is_none(),
                "{ext} has no real tree-sitter grammar dependency and should not carry a \
                 fabricated highlighter"
            );
        }
    }

    /// [`extension_for_fence_language`] must only ever return an extension that really exists in
    /// [`EXTENSIONS`] - it is the input to two different real grammar/highlighter lookups, both of
    /// which silently render plain text for an extension they don't recognize, so a typo here
    /// would be invisible at runtime.
    #[test]
    fn every_fence_alias_names_a_real_registry_extension() {
        for tag in [
            "rust",
            "rs",
            "python",
            "py",
            "typescript",
            "ts",
            "tsx",
            "javascript",
            "js",
            "jsx",
            "go",
            "golang",
            "json",
            "yaml",
            "yml",
            "toml",
            "c",
            "h",
            "html",
            "htm",
            "css",
            "markdown",
            "md",
        ] {
            let extension = extension_for_fence_language(tag)
                .unwrap_or_else(|| panic!("fence tag `{tag}` should resolve"));
            let entry = entry_for_extension(Some(extension)).unwrap_or_else(|| {
                panic!("fence tag `{tag}` resolved to `{extension}`, which is not a real entry")
            });
            assert!(
                entry.highlighter.is_some(),
                "fence tag `{tag}` resolves to `{extension}`, which has no real highlighter - a \
                 fence tagged that way would silently render as plain text"
            );
        }
    }

    /// Fence tags are author-written free text, and the two things that must never happen are a
    /// panic and a fabricated match. An unknown tag - and an empty one, which is what a bare
    /// ` ``` ` fence yields - is genuinely `None`.
    #[test]
    fn an_unknown_fence_tag_resolves_to_nothing() {
        assert_eq!(extension_for_fence_language("zig"), None);
        assert_eq!(extension_for_fence_language("console"), None);
        assert_eq!(extension_for_fence_language(""), None);
    }

    /// Fence tags are matched case-insensitively, matching [`entry_for_extension`]'s own
    /// case-insensitive extension lookup.
    #[test]
    fn fence_tags_are_matched_case_insensitively() {
        assert_eq!(extension_for_fence_language("HTML"), Some("html"));
        assert_eq!(extension_for_fence_language("Rust"), Some("rs"));
    }

    /// GitHub issue #154's own registry additions, pinned end to end: a real display name (the
    /// File view's status-bar label), a real chip, and a real highlighter - and deliberately no
    /// LSP, per this module's own scope docs.
    #[test]
    fn html_and_css_are_real_registry_entries_with_no_language_server() {
        for (extension, display_name) in [("html", "HTML"), ("htm", "HTML"), ("css", "CSS")] {
            let entry = entry_for_extension(Some(extension))
                .unwrap_or_else(|| panic!("{extension} should have a real registry entry"));
            assert_eq!(entry.display_name, display_name);
            assert!(entry.highlighter.is_some(), "{extension}");
            assert!(
                entry.lsp.is_none(),
                "{extension} must not spawn a language server - GitHub issue #154 asked for \
                 syntax support only"
            );
            assert!(entry.settings_row.is_none(), "{extension}");
        }
    }

    /// Every real Settings-page row must carry a real, non-empty `https://` install URL - not a
    /// placeholder, and not a bare label that would fail to open anything real via
    /// `crate::settings::widgets::open_command_for`. Pins the exact real, current five URLs
    /// (each individually verified against its own official source - see [`SettingsLspRow::
    /// install_url`]'s own docs) so a future edit that swaps one out is a deliberate, visible test
    /// diff rather than a silent drift to something unverified.
    #[test]
    fn every_settings_row_has_a_real_verified_https_install_url() {
        let mut urls: Vec<(&str, &str)> = EXTENSIONS
            .iter()
            .filter_map(|entry| entry.settings_row.map(|row| (row.binary, row.install_url)))
            .collect();
        urls.sort_unstable();
        assert_eq!(
            urls,
            vec![
                ("gopls", "https://go.dev/gopls/"),
                (
                    "pyright-langserver",
                    "https://github.com/microsoft/pyright/blob/main/docs/installation.md"
                ),
                (
                    "rust-analyzer",
                    "https://rust-analyzer.github.io/book/rust_analyzer_binary.html"
                ),
                (
                    "typescript-language-server",
                    "https://github.com/typescript-language-server/typescript-language-server"
                ),
                (
                    "vue-language-server",
                    "https://github.com/vuejs/language-tools"
                ),
            ]
        );
        for (binary, url) in &urls {
            assert!(
                url.starts_with("https://"),
                "{binary}'s install_url {url:?} should be a real https:// URL"
            );
        }
    }

    /// `.tsx`/`.jsx` genuinely need the TSX grammar variant, `.ts`/`.js` the plain one - proves
    /// the registry wires the *correct* one of the two real `crate::code_surface::code_view` wrapper fns per
    /// extension, not just "some highlighter or other".
    #[test]
    fn typescript_family_extensions_wire_the_correct_real_grammar_variant() {
        let ts = entry_for_extension(Some("ts")).expect("ts entry");
        let js = entry_for_extension(Some("js")).expect("js entry");
        let tsx = entry_for_extension(Some("tsx")).expect("tsx entry");
        let jsx = entry_for_extension(Some("jsx")).expect("jsx entry");

        // `std::ptr::fn_addr_eq`, not `==`/`assert_eq!` - a bare fn-pointer equality comparison
        // triggers `unpredictable_function_pointer_comparisons` (address identity for a real fn
        // item is stable enough for this real, same-binary test, but the compiler is honestly
        // right that `==` isn't the sanctioned way to ask that question).
        fn assert_same_fn(actual: Option<HighlighterFn>, expected: HighlighterFn, label: &str) {
            let actual = actual.unwrap_or_else(|| panic!("{label} should have a real highlighter"));
            assert!(
                std::ptr::fn_addr_eq(actual, expected),
                "{label} is wired to the wrong real highlighter variant"
            );
        }
        assert_same_fn(
            ts.highlighter,
            crate::code_surface::code_view::highlight_ts,
            "ts",
        );
        assert_same_fn(
            js.highlighter,
            crate::code_surface::code_view::highlight_ts,
            "js",
        );
        assert_same_fn(
            tsx.highlighter,
            crate::code_surface::code_view::highlight_tsx,
            "tsx",
        );
        assert_same_fn(
            jsx.highlighter,
            crate::code_surface::code_view::highlight_tsx,
            "jsx",
        );
    }

    /// The "Suggest auto-imports" toggle sends only what was directly verified to work, and only
    /// to the server it was verified against.
    ///
    /// Against a live `typescript-language-server` at the reported caret,
    /// `includeCompletionsForModuleExports: false` took the response from 1029 items carrying 7
    /// Node auto-import candidates to 1019 carrying none. `includePackageJsonAutoImports: "off"`,
    /// which sounds like it should do the same job, changed nothing at all - byte-identical
    /// response, all 7 still there - so it is deliberately not sent.
    #[test]
    fn the_auto_import_toggle_sends_only_the_preference_that_was_verified_to_work() {
        let options = auto_import_suppression_options("typescript-language-server")
            .expect("TypeScript is the one server this is wired for");
        assert_eq!(
            options["preferences"]["includeCompletionsForModuleExports"],
            serde_json::json!(false)
        );
        assert!(
            options["preferences"]
                .get("includePackageJsonAutoImports")
                .is_none(),
            "a preference observed to have no effect must not be sent as though it had one"
        );
        // The same binary, run as Vue's companion for the `<script>` half of an SFC.
        assert!(auto_import_suppression_options("typescript-language-server (vue)").is_some());
    }

    /// And nothing is sent on a guess. `pyright-langserver` really does honour
    /// `analysis.autoImportCompletions` (48 items down to 7, verified) - but over
    /// `workspace/configuration`, not here, and reaching that path needs a `lsp_core` API change
    /// this does not make. `rust-analyzer`'s own equivalent was never verified at all.
    #[test]
    fn a_server_whose_switch_is_unverified_is_sent_nothing() {
        for server in [
            "pyright-langserver",
            "rust-analyzer",
            "gopls",
            "vue-language-server",
        ] {
            assert_eq!(
                auto_import_suppression_options(server),
                None,
                "{server}: sending a plausible-looking key that was never checked is how a \
                 setting ends up quietly doing nothing"
            );
        }
    }

    /// The live-reported case this whole passthrough exists for, end to end.
    ///
    /// A browser project with `@types/node` installed gets Node's entire API offered as
    /// auto-imports; accepting one writes `import { appendFile } from 'node:fs'`, which is valid
    /// TypeScript (verified against a live server - it raises no diagnostic) and which no browser
    /// bundler can resolve. `tsconfig.json` cannot turn that off: it configures the *compiler*,
    /// while auto-import behaviour is a tsserver `UserPreferences` field that
    /// `typescript-language-server` reads from `initializationOptions.preferences` at startup
    /// (read directly out of the installed server:
    /// `mergeTsPreferences(userInitializationOptions.preferences || {})`). This app sent nothing
    /// there, so there was no way to reach it at all.
    #[test]
    fn a_user_can_suppress_node_auto_imports_that_no_tsconfig_could_reach() {
        let user: toml::Value = toml::from_str(
            "[preferences]\n\
             autoImportSpecifierExcludeRegexes = [\"^node:\", \"^fs$\"]\n\
             includePackageJsonAutoImports = \"off\"\n",
        )
        .expect("a real settings.toml fragment");
        // `typescript-language-server` has no built-in options of its own in this registry.
        let merged = merge_initialization_options(None, Some(&user))
            .expect("a user-supplied set must reach the handshake even with nothing to merge over");
        assert_eq!(
            merged["preferences"]["autoImportSpecifierExcludeRegexes"],
            serde_json::json!(["^node:", "^fs$"])
        );
        assert_eq!(
            merged["preferences"]["includePackageJsonAutoImports"],
            serde_json::json!("off")
        );
    }

    /// The merge must never cost a server the options this registry resolved for it - Pyright's
    /// real `pythonPath` is discovered by probing the filesystem and cannot be reconstructed from
    /// a config file, so a user setting one unrelated key must leave it alone.
    #[test]
    fn a_user_override_adds_to_the_registrys_own_options_rather_than_replacing_them() {
        let built_in = serde_json::json!({
            "python": {
                "pythonPath": "/usr/bin/python3",
                "analysis": { "autoSearchPaths": true },
            },
        });
        let user: toml::Value =
            toml::from_str("[python.analysis]\ntypeCheckingMode = \"strict\"\n")
                .expect("a real settings.toml fragment");
        let merged = merge_initialization_options(Some(built_in), Some(&user))
            .expect("both halves are present");
        assert_eq!(
            merged["python"]["pythonPath"],
            serde_json::json!("/usr/bin/python3"),
            "a real, probed path the user never mentioned must survive their edit"
        );
        assert_eq!(
            merged["python"]["analysis"]["autoSearchPaths"],
            serde_json::json!(true),
            "and so must a sibling key inside the object they did edit"
        );
        assert_eq!(
            merged["python"]["analysis"]["typeCheckingMode"],
            serde_json::json!("strict"),
            "while what they actually set is applied"
        );
    }

    /// A user value wins at the leaf, and an array is a leaf: half-merging two arrays is nobody's
    /// intent, and would produce a list neither side asked for.
    #[test]
    fn a_user_supplied_scalar_or_array_replaces_rather_than_blends() {
        let built_in = serde_json::json!({ "flags": ["a", "b"], "level": 1 });
        let user: toml::Value =
            toml::from_str("flags = [\"c\"]\nlevel = 2\n").expect("a real fragment");
        let merged =
            merge_initialization_options(Some(built_in), Some(&user)).expect("both present");
        assert_eq!(merged["flags"], serde_json::json!(["c"]));
        assert_eq!(merged["level"], serde_json::json!(2));
    }

    /// And with nothing configured, every server's handshake is byte-for-byte what it was.
    #[test]
    fn no_user_settings_leaves_a_servers_own_options_exactly_as_they_were() {
        let built_in = serde_json::json!({ "python": { "pythonPath": "/usr/bin/python3" } });
        assert_eq!(
            merge_initialization_options(Some(built_in.clone()), None),
            Some(built_in)
        );
        assert_eq!(merge_initialization_options(None, None), None);
    }
}
