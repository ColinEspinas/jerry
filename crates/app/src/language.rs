//! The single, canonical per-extension language registry - Revision R8's consolidation of what
//! used to be four independently-maintained tables that could silently drift against each other:
//! `crate::settings::LSP_LANGUAGES` (the Language servers settings page),
//! `crate::file_tree::lang_chip_for_name` (file-tree chip colors), `crate::code_view`'s
//! `language_name_for_extension` (the File view's status-bar language label), and
//! `crate::root::code_surface`'s old `is_rust` boolean gate (whether to spawn/talk to an LSP
//! client at all). All four now read from [`entry_for_extension`]/[`EXTENSIONS`] below - matching
//! this codebase's Revision R5.5-established preference for one real table over several that can
//! drift, not a forced abstraction (each entry is still just a few plain fields, no trait
//! machinery).
//!
//! ## Scope: which languages actually spawn a server here
//!
//! `lsp: Some(..)` for Rust/TypeScript-family/Python - real, live-tested end to end (see
//! `lsp_core::client`'s own tests for Rust, and this crate's `language::tests` /
//! `root::lsp`-adjacent integration tests for TypeScript/Python). `lsp: None` for TOML/Markdown/
//! SQL (never had a server), Go (the user's explicit ask named TypeScript/Vue/Python, not Go;
//! `gopls` stays PATH-detection-only on the Settings page, matching its pre-existing "not
//! installed" real state there), and **Vue** - a deliberate, evidence-backed scope-down, not an
//! oversight:
//!
//! Live-probing the sandbox's actual installed `@vue/language-server@3.3.8` (`vue-language-server
//! --stdio`, no `--tsdk`) showed it completes a real `initialize` handshake and advertises
//! `hoverProvider`/`definitionProvider`/etc., but **hard-crashes** (uncaught `TypeError: Cannot
//! read properties of undefined (reading 'protocol')` in `@vue/language-server/lib/server.js`,
//! process exit) the instant it tries to compute diagnostics for *any* `.vue` file - even one
//! with no `<script>` block at all.
//!
//! Correction (a previous version of this doc comment got the evidence backwards): that crash is
//! specifically what happens when `--tsdk` is **omitted**, not when it's passed. A real `ls -la`
//! on this sandbox's actual TypeScript `lib/` directory shows a genuine, present `typescript.js`
//! (9,112,572 bytes) - the earlier claim that no real `typescript.js` exists at that path for
//! either a project-local `npm install typescript` or the server's own bundled copy was simply
//! false, and live-spawning `vue-language-server --stdio --tsdk=<that real path>` does **not**
//! crash at startup the way omitting the flag does.
//!
//! The real reason Vue support is still deferred, live-verified separately: even *with* `--tsdk`
//! correctly set (avoiding the startup crash above), the real installed server's "hybrid mode"
//! then sends a real `tsserver/request` (e.g. for `_vue:projectInfo`) expecting a companion
//! `typescript-language-server` process (running `@vue/typescript-plugin`) to answer it over
//! custom `tsserver/request`/`tsserver/response` notifications - and no such companion process
//! exists in this codebase's current architecture, so real diagnostics still never arrive even
//! once the startup crash itself is avoided. Building that real two-process coordination (a
//! second LSP client, wired specifically to talk Vue-specific `tsserver/*` notifications to the
//! first) is genuine new architecture this phase's other priorities (generalizing `lsp-core`
//! itself, real TypeScript/Python support) shouldn't be sacrificed for - so Vue's
//! [`ExtensionEntry::lsp`] stays `None`. PATH-detection (`crate::settings::LSP_LANGUAGES`,
//! unaffected by this) and the chip color below still work regardless, per this phase's own
//! stated minimum bar.

use std::path::Path;

use gpui::Rgba;
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
}

/// Which of `crate::code_view`'s real `tree-sitter`-backed syntax highlighters applies to an
/// extension, if any - see [`ExtensionEntry::highlighter`]'s own docs for why this lives on the
/// registry itself rather than in a second, independent table.
pub type HighlighterFn = fn(&str) -> Vec<crate::code_view::HighlightSpan>;

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
    /// `(fg, bg)`, straight from `crate::theme::lang`.
    pub chip_colors: (Rgba, Rgba),
    pub lsp: Option<LspIdentity>,
    /// The Settings "Language servers" page's own real row for this language - `Some` for
    /// exactly the five extensions that page shows one row per (`rs`/`ts`/`vue`/`py`/`go`, one
    /// row per *family*, not per extension - `.tsx`/`.js`/`.jsx` share TypeScript's row), `None`
    /// for every other entry (TOML/Markdown/SQL never had a server; `.tsx`/`.js`/`.jsx` aren't a
    /// second, redundant TypeScript row). Carries the real detection binary independently of
    /// [`Self::lsp`] - Vue/Go are real, `$PATH`-detectable binaries this phase deliberately
    /// doesn't spawn a live client for (see this module's top-level docs), so the Settings page
    /// still needs a binary name for them even where [`Self::lsp`] is `None`.
    pub settings_row: Option<SettingsLspRow>,
    /// Which real `crate::code_view` highlighter function parses this extension's real syntax -
    /// `None` for an extension with no `tree-sitter` grammar wired at all (TOML/Markdown/SQL, and
    /// Vue - highlighting is unrelated to and independent of [`Self::lsp`]'s LSP scope-down, see
    /// this module's top-level docs). Previously `crate::code_view::load_file` maintained its own
    /// second, independent extension -> highlighter `match` that this same registry knew nothing
    /// about - a real, live gap where a new registered language could silently render as plain
    /// text with no compile error or test failure to catch it. This field, plus
    /// `crate::code_view::highlighter_for_extension` reading from it, is that gap closed: one
    /// real table, not two that can drift.
    pub highlighter: Option<HighlighterFn>,
}

/// See [`ExtensionEntry::settings_row`].
#[derive(Debug, Clone, Copy)]
pub struct SettingsLspRow {
    pub binary: &'static str,
    /// Generic descriptive copy, not a live count - see `crate::settings::LspLanguage::note`'s
    /// own docs (this struct is `crate::settings::LSP_LANGUAGES`' one real source now).
    pub note: &'static str,
    /// The real, official install/docs page for this server - what the Settings -> Language
    /// servers page's "Install" action (a genuinely `not installed` row only, see
    /// `crate::settings::LspRow::is_ready`) opens in the user's default browser via
    /// `crate::root::settings_widgets::open_command_for`. Deliberately the server's own official
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

/// Pyright expects a real, non-empty `initializationOptions` block up front (unlike
/// rust-analyzer/typescript-language-server, which behave fine with none) - see this crate's
/// `lsp-core` generalization docs. Real `pythonPath` detection via `pty_core::resolve_on_path`
/// (the same real `$PATH` search `crate::settings::detect_lsp_rows` already uses), not a
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
};

const PYRIGHT_LSP: LspIdentity = LspIdentity {
    binary: "pyright-langserver",
    args: &["--stdio"],
    initialization_options: pyright_initialization_options,
    workspace_configuration: pyright_workspace_configuration,
};

const RUST_ANALYZER_LSP: LspIdentity = LspIdentity {
    binary: "rust-analyzer",
    args: &[],
    initialization_options: no_initialization_options,
    workspace_configuration: lsp_core::default_workspace_configuration,
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
        highlighter: Some(crate::code_view::highlight_rust),
    },
    ExtensionEntry {
        extension: "toml",
        display_name: "TOML",
        lsp_language_id: "toml",
        chip_label: "to",
        chip_colors: theme::lang::TOML,
        lsp: None,
        settings_row: None,
        highlighter: None,
    },
    ExtensionEntry {
        extension: "md",
        display_name: "Markdown",
        lsp_language_id: "markdown",
        chip_label: "md",
        chip_colors: theme::lang::MD,
        lsp: None,
        settings_row: None,
        highlighter: None,
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
        highlighter: Some(crate::code_view::highlight_ts),
    },
    ExtensionEntry {
        extension: "tsx",
        display_name: "TypeScript (TSX)",
        lsp_language_id: "typescriptreact",
        chip_label: "ts",
        chip_colors: theme::lang::TS,
        lsp: Some(TYPESCRIPT_LSP),
        settings_row: None,
        highlighter: Some(crate::code_view::highlight_tsx),
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
        // JavaScript) rather than `highlight_tsx` - see `crate::code_view::highlight_typescript`'s
        // own docs.
        highlighter: Some(crate::code_view::highlight_ts),
    },
    ExtensionEntry {
        extension: "jsx",
        display_name: "JavaScript (JSX)",
        lsp_language_id: "javascriptreact",
        chip_label: "ts",
        chip_colors: theme::lang::TS,
        lsp: Some(TYPESCRIPT_LSP),
        settings_row: None,
        highlighter: Some(crate::code_view::highlight_tsx),
    },
    ExtensionEntry {
        extension: "vue",
        display_name: "Vue",
        lsp_language_id: "vue",
        chip_label: "vue",
        chip_colors: theme::lang::VUE,
        // Deliberately `None` - see this module's top-level docs for the real, evidence-backed
        // reason (a reproducible upstream crash, not an oversight).
        lsp: None,
        settings_row: Some(SettingsLspRow {
            binary: "vue-language-server",
            // Honest, not the old "starts when a .vue file opens" claim - this phase's client
            // isn't wired for `.vue` at all (see this module's top-level docs for the real,
            // reproduced crash that's why), so the note must not promise it does.
            note: "detected on PATH; live analysis not wired this phase (see docs)",
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
        highlighter: Some(crate::code_view::highlight_python),
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
        // No real Go `tree-sitter` grammar dependency in this workspace.
        highlighter: None,
    },
];

/// The real, canonical source for `crate::settings::LSP_LANGUAGES` - every [`ExtensionEntry`]
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
        Some(entry) => (entry.chip_label, entry.chip_colors.0, entry.chip_colors.1),
        None => (".", theme::lang::UNKNOWN.0, theme::lang::UNKNOWN.1),
    }
}

/// The real `textDocument/didOpen` `language_id` for `extension`, if this crate would spawn an
/// LSP client for it at all (`None` for an extension with no [`ExtensionEntry::lsp`], e.g.
/// `.vue`/`.go`, or one absent from [`EXTENSIONS`] entirely) - the replacement for the old
/// `is_rust` boolean gate in `crate::root::code_surface`: "is there an `lsp_language_id` for this
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

/// Builds a real, owned [`ServerSpawnConfig`] for `extension`, if this crate spawns a server for
/// it - `None` for an extension with no [`ExtensionEntry::lsp`] entry. Calls
/// [`LspIdentity::initialization_options`] fresh each time (see that field's own docs on why
/// that's cheap and correctly *not* memoized: it can depend on real, possibly-changed PATH state
/// at the moment of a real spawn).
pub fn server_spawn_config(extension: Option<&str>) -> Option<ServerSpawnConfig> {
    let lsp = entry_for_extension(extension)?.lsp?;
    Some(ServerSpawnConfig {
        name: lsp.binary,
        binary: lsp.binary,
        args: lsp.args.iter().map(|arg| arg.to_string()).collect(),
        initialization_options: (lsp.initialization_options)(),
        workspace_configuration: lsp.workspace_configuration,
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
        assert_eq!((fg, bg), theme::lang::RS);

        let (label, fg, bg) = chip_for_extension(Some("xyz"));
        assert_eq!(label, ".");
        assert_eq!((fg, bg), theme::lang::UNKNOWN);
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

    #[test]
    fn vue_and_go_are_detected_but_never_spawn_a_real_client() {
        assert!(lsp_language_id_for_extension(Some("vue")).is_none());
        assert!(lsp_binary_for_extension(Some("vue")).is_none());
        assert!(server_spawn_config(Some("vue")).is_none());
        assert!(lsp_language_id_for_extension(Some("go")).is_none());
        // The chip still resolves for real regardless (this phase's stated minimum bar).
        assert_eq!(chip_for_extension(Some("vue")).0, "vue");
        assert_eq!(chip_for_extension(Some("go")).0, "go");
    }

    #[test]
    fn rust_typescript_and_python_all_produce_a_real_spawn_config() {
        for ext in ["rs", "ts", "py"] {
            let config = server_spawn_config(Some(ext))
                .unwrap_or_else(|| panic!("{ext} should have a real spawn config"));
            assert!(!config.binary.is_empty());
            assert_eq!(config.name, config.binary);
        }
    }

    #[test]
    fn pyright_gets_real_non_null_initialization_options() {
        let config = server_spawn_config(Some("py")).expect("python spawn config");
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
    /// registry - not a second, independent table `crate::code_view::load_file` used to maintain
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
            vec!["js", "jsx", "py", "rs", "ts", "tsx"],
            "this is the real, current set of extensions with a genuine tree-sitter grammar \
             dependency - a change here should be a deliberate decision, not a silent drift"
        );
    }

    /// The other half of the same guard: extensions with no real `tree-sitter` grammar dependency
    /// in this workspace (TOML/Markdown/SQL never had one; Vue/Go's lack of one is independent of
    /// their separate LSP scope-down) must not carry a stray, fabricated `highlighter`.
    #[test]
    fn extensions_with_no_real_grammar_have_no_highlighter_wired() {
        for ext in ["toml", "md", "sql", "vue", "go"] {
            let entry = entry_for_extension(Some(ext))
                .unwrap_or_else(|| panic!("{ext} should have a real registry entry"));
            assert!(
                entry.highlighter.is_none(),
                "{ext} has no real tree-sitter grammar dependency and should not carry a \
                 fabricated highlighter"
            );
        }
    }

    /// Every real Settings-page row must carry a real, non-empty `https://` install URL - not a
    /// placeholder, and not a bare label that would fail to open anything real via
    /// `crate::root::settings_widgets::open_command_for`. Pins the exact real, current five URLs
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
    /// the registry wires the *correct* one of the two real `crate::code_view` wrapper fns per
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
        assert_same_fn(ts.highlighter, crate::code_view::highlight_ts, "ts");
        assert_same_fn(js.highlighter, crate::code_view::highlight_ts, "js");
        assert_same_fn(tsx.highlighter, crate::code_view::highlight_tsx, "tsx");
        assert_same_fn(jsx.highlighter, crate::code_view::highlight_tsx, "jsx");
    }
}
