//! Bundles the two real font families `design_handoff_jerry_ade/README.md`'s "Design
//! tokens" section requires ("Fonts: IBM Plex Sans ... and IBM Plex Mono ... Nothing
//! else. Both are OFL - bundle them.") and registers them with GPUI's real text system.
//!
//! ## Source
//!
//! The `.ttf` files under `assets/fonts/` are the real, unmodified static weights from
//! IBM's own upstream releases (`github.com/IBM/plex`, the canonical OFL-licensed
//! source Zed's own bundled `IBM Plex Sans` also comes from - see
//! `vendor/zed/assets/fonts/ibm-plex-sans/`): `@ibm/plex-sans@1.1.0`'s
//! `ibm-plex-sans.zip` and `@ibm/plex-mono@2.5.0`'s `ibm-plex-mono.zip` release assets,
//! `fonts/complete/ttf/`. Every weight the design handoff requires (see `crate::theme`'s
//! `font` module docs) has a real static file upstream - no "closest available weight"
//! substitution was needed for those:
//!
//! | Weight | Sans file | Mono file |
//! |---|---|---|
//! | 400 | `IBMPlexSans-Regular.ttf` | `IBMPlexMono-Regular.ttf` |
//! | 450 | `IBMPlexSans-Text.ttf` | `IBMPlexMono-Text.ttf` |
//! | 500 | `IBMPlexSans-Medium.ttf` | `IBMPlexMono-Medium.ttf` |
//! | 600 | `IBMPlexSans-SemiBold.ttf` | `IBMPlexMono-SemiBold.ttf` (bundled for ANSI bold, see below) |
//!
//! Each of `assets/fonts/ibm-plex-{sans,mono}/LICENSE.txt` is IBM's real, unmodified SIL
//! Open Font License 1.1 text, copied alongside the files it covers.
//!
//! ## `IBMPlexMono-SemiBold.ttf`: bundled for ANSI bold, not a design-token weight
//!
//! The design handoff's own weight table only calls for Mono 400/450/500 - 600 isn't a
//! Jerry UI weight. It's bundled anyway because `crate::terminal_pane::render_row` asks
//! for `FontWeight::BOLD` (700) whenever a cell's ANSI `SGR 1` (bold) flag is set, and
//! without *some* bundled Mono weight above 500, GPUI's real weight-matching
//! (`cosmic_text_system.rs`'s `find_best_match`) would silently resolve every "bold"
//! terminal cell to whichever bundled weight is numerically closest to 700 - which, before
//! this file was added, was 500 (Medium): visually not bold at all, a real regression from
//! the plain system-monospace fallback this pane used before real fonts were bundled (a
//! system fallback almost always has a real 700 weight). 600 (SemiBold) is the closest
//! *available* weight to 700 once bundled, per the same "closest available weight" leeway
//! the design handoff itself allows - see `crate::terminal_pane::render_row`'s own doc
//! comment for where `FontWeight::BOLD` is requested.
//!
//! ## Unused-for-now, not dead code
//!
//! Nothing in this app yet requests any weight other than the default (`FontWeight::NORMAL`,
//! 400) and - for ANSI-bold terminal cells - `FontWeight::BOLD` (700, resolving to the
//! 600 file above). Sans 450/500/600 and Mono 450 are real, loaded, and available the
//! moment a later phase's UI actually calls `.font_weight(..)` for them (the session
//! rail, tab strip, etc. per the design handoff's type scale) - they are groundwork, not
//! leftover dead code a cleanup pass should remove.
//!
//! ## Weight resolution, verified against the real pinned `fontdb`
//!
//! A static `.ttf`'s *legacy* family name (name table ID 1) is unique per weight (e.g.
//! `"IBM Plex Sans Medm"` for the Medium file) - only its *typographic* family (name ID
//! 16) is the shared `"IBM Plex Sans"`. Verified with `fontTools` against every file
//! bundled here that the non-Regular weights do carry a real ID 16 (`"IBM Plex
//! Sans"`/`"IBM Plex Mono"`) plus the correct `OS/2.usWeightClass` (450/500/600), and
//! that `fontdb` 0.23.0 - the exact version this workspace's `Cargo.lock` resolves for
//! `cosmic-text` 0.19.0, which backs GPUI's real Linux text system
//! (`vendor/zed/crates/gpui_wgpu/src/cosmic_text_system.rs`, `impl PlatformTextSystem`) -
//! prefers ID 16 over ID 1 when building each face's family list
//! (`fontdb-0.23.0/src/lib.rs`'s `parse_names`: `collect_families(TYPOGRAPHIC_FAMILY,
//! ..)`, falling back to `FAMILY` only when empty). So a `Font { family: "IBM Plex
//! Sans".into(), weight: FontWeight(500.0), .. }` query really does resolve every
//! bundled weight as one family, distinguished by real weight matching
//! (`cosmic_text_system.rs`'s `find_best_match`, `font_kit::matching::find_best_match`
//! against each candidate's real `OS/2.usWeightClass`) - not a guess.
//!
//! ## Registration, verified against `vendor/zed/crates/zed/src/main.rs`
//!
//! [`Assets`] implements the real `gpui::AssetSource` trait (`vendor/zed/crates/gpui/src/
//! assets.rs:13`); [`load_embedded_fonts`] mirrors Zed's own `load_embedded_fonts`
//! (`vendor/zed/crates/zed/src/main.rs:1806`) almost exactly: list `"fonts"` via
//! `cx.asset_source()`, load each entry's real bytes, and hand them to the real
//! `cx.text_system().add_fonts(..)` (`vendor/zed/crates/gpui/src/text_system.rs:101`).
//! `crate::run` wires `Assets` in via `Application::with_assets` (`vendor/zed/crates/
//! gpui/src/app.rs:198`, the same method Zed's own `main.rs:343` uses) before opening the
//! window, and calls [`load_embedded_fonts`] as the very first thing inside the launch
//! callback - see that function's docs for why a failure there is logged, not
//! `.unwrap()`ed.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};

/// `(asset path, real font bytes)` for every bundled weight - see the module docs' table.
/// `include_bytes!` embeds the real file contents into the binary at compile time, so
/// there is no runtime dependency on `assets/fonts/` existing on disk once built.
const FONT_FILES: &[(&str, &[u8])] = &[
    (
        "fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf",
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"),
    ),
    (
        "fonts/ibm-plex-sans/IBMPlexSans-Text.ttf",
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Text.ttf"),
    ),
    (
        "fonts/ibm-plex-sans/IBMPlexSans-Medium.ttf",
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Medium.ttf"),
    ),
    (
        "fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf",
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"),
    ),
    (
        "fonts/ibm-plex-mono/IBMPlexMono-Regular.ttf",
        include_bytes!("../../../assets/fonts/ibm-plex-mono/IBMPlexMono-Regular.ttf"),
    ),
    (
        "fonts/ibm-plex-mono/IBMPlexMono-Text.ttf",
        include_bytes!("../../../assets/fonts/ibm-plex-mono/IBMPlexMono-Text.ttf"),
    ),
    (
        "fonts/ibm-plex-mono/IBMPlexMono-Medium.ttf",
        include_bytes!("../../../assets/fonts/ibm-plex-mono/IBMPlexMono-Medium.ttf"),
    ),
    (
        "fonts/ibm-plex-mono/IBMPlexMono-SemiBold.ttf",
        include_bytes!("../../../assets/fonts/ibm-plex-mono/IBMPlexMono-SemiBold.ttf"),
    ),
];

/// A real [`gpui::AssetSource`] serving only the bundled font files above - this app has
/// no other assets (per the design handoff's "Assets: None ... every icon is composed
/// from rects and text glyphs").
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        for (name, bytes) in FONT_FILES {
            if *name == path {
                return Ok(Some(Cow::Borrowed(*bytes)));
            }
        }
        Ok(None)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path != "fonts" {
            return Ok(Vec::new());
        }
        Ok(FONT_FILES
            .iter()
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

/// Loads every bundled font into GPUI's real text system, so `theme::font::SANS`/
/// `theme::font::MONO` resolve to the real bundled glyphs rather than silently falling
/// back to a system default. Returns an error (never panics) if a bundled asset is
/// missing or GPUI's text system rejects it - the caller logs and continues rather than
/// treating a font-loading failure as fatal to the whole app (matching this workspace's
/// "no `.unwrap()`/`.expect()` outside `#[cfg(test)]`" rule).
pub fn load_embedded_fonts(cx: &App) -> Result<()> {
    let asset_source = cx.asset_source().clone();
    let mut embedded_fonts = Vec::new();
    for path in asset_source.list("fonts")? {
        if let Some(bytes) = asset_source.load(&path)? {
            embedded_fonts.push(bytes);
        }
    }
    cx.text_system().add_fonts(embedded_fonts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_exactly_the_bundled_font_paths() {
        let assets = Assets;
        let listed = assets.list("fonts").expect("list should not fail");
        assert_eq!(listed.len(), FONT_FILES.len());
        for (name, _) in FONT_FILES {
            assert!(
                listed.iter().any(|entry| entry.as_ref() == *name),
                "expected {name} to be listed"
            );
        }
    }

    #[test]
    fn lists_nothing_for_an_unrelated_path() {
        let assets = Assets;
        let listed = assets.list("icons").expect("list should not fail");
        assert!(listed.is_empty());
    }

    #[test]
    fn loads_real_nonempty_font_bytes_for_every_bundled_path() {
        let assets = Assets;
        for (name, _) in FONT_FILES {
            let bytes = assets
                .load(name)
                .expect("load should not fail")
                .unwrap_or_else(|| panic!("expected {name} to be present"));
            // A real TrueType font starts with one of a small set of sfnt version tags;
            // `0x00010000` (the plain TrueType tag) is what every file bundled here uses.
            assert!(
                bytes.len() > 1024,
                "{name} is suspiciously small: {} bytes",
                bytes.len()
            );
            assert_eq!(
                &bytes[0..4],
                &[0x00, 0x01, 0x00, 0x00],
                "{name} does not start with the TrueType sfnt version tag"
            );
        }
    }

    #[test]
    fn returns_none_for_a_path_not_in_the_bundle() {
        let assets = Assets;
        assert!(assets
            .load("fonts/does-not-exist.ttf")
            .expect("load should not fail")
            .is_none());
    }

    /// Checks every bundled file's real name table (ID 16, falling back to ID 1 exactly
    /// the way `fontdb::parse_names` does - see the module docs' "Weight resolution"
    /// section) and `OS/2.usWeightClass` directly against the embedded bytes, via
    /// `ttf_parser` (the same crate `fontdb` itself is built on). This is what makes the
    /// module docs' weight table and "Weight resolution" claims checkable by `cargo test`
    /// rather than only by a one-time `fontTools` inspection that could silently drift out
    /// of sync with the actual bundled files (e.g. a future edit that swaps in the wrong
    /// weight, or a copy-paste error in the doc table).
    #[test]
    fn bundled_font_weights_and_family_names_match_the_module_docs() {
        let expectations: &[(&str, &str, u16)] = &[
            (
                "fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf",
                "IBM Plex Sans",
                400,
            ),
            (
                "fonts/ibm-plex-sans/IBMPlexSans-Text.ttf",
                "IBM Plex Sans",
                450,
            ),
            (
                "fonts/ibm-plex-sans/IBMPlexSans-Medium.ttf",
                "IBM Plex Sans",
                500,
            ),
            (
                "fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf",
                "IBM Plex Sans",
                600,
            ),
            (
                "fonts/ibm-plex-mono/IBMPlexMono-Regular.ttf",
                "IBM Plex Mono",
                400,
            ),
            (
                "fonts/ibm-plex-mono/IBMPlexMono-Text.ttf",
                "IBM Plex Mono",
                450,
            ),
            (
                "fonts/ibm-plex-mono/IBMPlexMono-Medium.ttf",
                "IBM Plex Mono",
                500,
            ),
            (
                "fonts/ibm-plex-mono/IBMPlexMono-SemiBold.ttf",
                "IBM Plex Mono",
                600,
            ),
        ];
        assert_eq!(
            expectations.len(),
            FONT_FILES.len(),
            "this test's expectations table is out of sync with FONT_FILES - a bundled \
             file was added or removed without updating this test"
        );

        let assets = Assets;
        for (path, expected_family, expected_weight) in expectations {
            let bytes = assets
                .load(path)
                .expect("load should not fail")
                .unwrap_or_else(|| panic!("expected {path} to be present"));
            let face = ttf_parser::Face::parse(&bytes, 0)
                .unwrap_or_else(|err| panic!("{path} failed to parse as a font: {err:?}"));

            assert_eq!(
                face.weight().to_number(),
                *expected_weight,
                "{path}: OS/2.usWeightClass didn't match the module docs"
            );

            // `fontdb::parse_names`'s exact preference order: the typographic family
            // (name ID 16) if present, otherwise the legacy family (name ID 1).
            let typographic_family = face.names().into_iter().find(|name| {
                name.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY && name.is_unicode()
            });
            let legacy_family = face
                .names()
                .into_iter()
                .find(|name| name.name_id == ttf_parser::name_id::FAMILY && name.is_unicode());
            let resolved_family = typographic_family
                .or(legacy_family)
                .and_then(|name| name.to_string())
                .unwrap_or_else(|| panic!("{path}: no usable family name found"));

            assert_eq!(
                &resolved_family, expected_family,
                "{path}: family name `fontdb` would resolve didn't match the module docs"
            );
        }
    }
}
