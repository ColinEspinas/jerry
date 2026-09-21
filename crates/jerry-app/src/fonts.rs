//! Bundles the two font families the design requires (IBM Plex Sans, IBM Plex Mono; both OFL)
//! and registers them with GPUI's text system.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};

/// `(asset path, embedded bytes)` for every bundled weight - see the module docs' table.
/// `include_bytes!` embeds the files at compile time, so there's no runtime dependency on
/// `assets/fonts/` existing on disk once built.
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

/// This app's one [`gpui::AssetSource`], covering both bundled asset families: the fonts above
/// (`fonts/...`) and the shipped Phosphor icons (`icons/...`, GitHub issue #282 -
/// `crate::icons::ICON_FILES` owns that list, this only routes to it).
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        for (name, bytes) in FONT_FILES {
            if *name == path {
                return Ok(Some(Cow::Borrowed(*bytes)));
            }
        }
        Ok(crate::icons::load_asset(path))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        match path {
            "fonts" => Ok(FONT_FILES
                .iter()
                .map(|(name, _)| SharedString::from(*name))
                .collect()),
            "icons" => Ok(crate::icons::ICON_FILES
                .iter()
                .map(|(name, _)| SharedString::from(*name))
                .collect()),
            _ => Ok(Vec::new()),
        }
    }
}

/// Loads every bundled font into GPUI's text system, so `theme::font::SANS`/`theme::font::MONO`
/// resolve to the bundled glyphs instead of silently falling back to a system default. Returns
/// an error rather than panicking if a bundled asset is missing or the text system rejects it -
/// the caller logs and continues rather than treating a font-loading failure as fatal to the
/// whole app (this workspace's "no `.unwrap()`/`.expect()` outside `#[cfg(test)]`" rule).
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
        let listed = assets.list("themes").expect("list should not fail");
        assert!(listed.is_empty());
    }

    #[test]
    fn also_serves_the_shipped_icons_alongside_the_fonts() {
        let assets = Assets;
        let listed = assets.list("icons").expect("list should not fail");
        assert_eq!(listed.len(), crate::icons::ICON_FILES.len());
        assert!(!listed.is_empty(), "sanity check: icons really are bundled");

        for (name, _) in crate::icons::ICON_FILES {
            let bytes = assets
                .load(name)
                .expect("load should not fail")
                .unwrap_or_else(|| panic!("expected {name} to be served"));
            assert!(bytes.starts_with(b"<svg"), "{name} is not an SVG");
        }
    }

    #[test]
    fn returns_none_for_an_icon_path_not_in_the_bundle() {
        let assets = Assets;
        assert!(assets
            .load("icons/does-not-exist.svg")
            .expect("load should not fail")
            .is_none());
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
