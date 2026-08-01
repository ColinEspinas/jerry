//! Custom icon packs (GitHub issue #5's "custom icon packs" acceptance criterion) - an optional,
//! real, user-supplied directory of SVG icons that override this app's own default, built-in
//! icons (styled `div()` shapes and Unicode glyphs, never image assets on their own). No active
//! pack, or no matching file in the active pack, always falls back to the existing default
//! rendering - this module only ever *adds* an override path, never removes the built-in look.
//!
//! ## Scope
//!
//! Only `crate::work_surface::state::agent_tint`/`agent_initial`'s three agent-kind chips
//! (Claude/Codex/Shell - `crate::work_surface::render::render_agent_icon`'s own real call
//! sites) are wired to check a pack today. The other ~25 distinct icon concepts cataloged across
//! this app (file-type chips, folder icons, chevrons, status dots, ...) are a real, stated
//! follow-up, not yet wired - picking one well-scoped, high-value integration point first rather
//! than touching every render call site in one pass.
//!
//! ## Real icon files, not a theme tint
//!
//! Unlike this app's own bundled UI icons (styled shapes/glyphs that all pick up a theme color
//! token), a user-supplied pack icon is rendered via `gpui::svg().external_path(..)` with **no**
//! `.text_color()` tint applied - a real, user-chosen brand icon (e.g. a real Claude/Codex logo)
//! keeps whatever colors its own SVG file defines, rather than being flattened to a single
//! theme-matched fill the way this app's own icon-font-free UI chrome is.

use std::path::PathBuf;

use crate::settings::store::IconPackSettings;

/// The real, on-disk path to `name`'s icon in the active pack, if
/// [`IconPackSettings::directory`] is set and a file named `<name>.svg` really exists inside it.
/// Returns `None` (never a fabricated path) for no active pack, a pack directory with no
/// matching file, or a matching name that isn't actually a real file (checked with a real
/// `Path::is_file` call, not just string concatenation, since a stale or misconfigured
/// directory is a real possibility this must not silently paper over with a broken `svg()`
/// element pointed at a directory or a symlink to nowhere).
pub fn resolve_icon(pack: &IconPackSettings, name: &str) -> Option<PathBuf> {
    let directory = pack.directory.as_ref()?;
    let candidate = directory.join(format!("{name}.svg"));
    candidate.is_file().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_icon_is_none_with_no_pack_configured() {
        let pack = IconPackSettings { directory: None };
        assert_eq!(resolve_icon(&pack, "claude"), None);
    }

    #[test]
    fn resolve_icon_is_none_when_the_named_file_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = IconPackSettings {
            directory: Some(dir.path().to_path_buf()),
        };
        assert_eq!(resolve_icon(&pack, "claude"), None);
    }

    #[test]
    fn resolve_icon_finds_a_real_matching_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.svg"), "<svg></svg>").expect("write");
        let pack = IconPackSettings {
            directory: Some(dir.path().to_path_buf()),
        };
        assert_eq!(
            resolve_icon(&pack, "claude"),
            Some(dir.path().join("claude.svg"))
        );
    }

    #[test]
    fn resolve_icon_ignores_a_directory_with_the_matching_name() {
        // A directory named `claude.svg` is not a real icon file - `is_file` must reject it
        // rather than handing back a path `svg()` can't actually load.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("claude.svg")).expect("mkdir");
        let pack = IconPackSettings {
            directory: Some(dir.path().to_path_buf()),
        };
        assert_eq!(resolve_icon(&pack, "claude"), None);
    }

    #[test]
    fn resolve_icon_is_scoped_to_the_exact_requested_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("codex.svg"), "<svg></svg>").expect("write");
        let pack = IconPackSettings {
            directory: Some(dir.path().to_path_buf()),
        };
        assert_eq!(
            resolve_icon(&pack, "claude"),
            None,
            "a pack having *some* icons must never make an unrelated name resolve to one of them"
        );
    }
}
