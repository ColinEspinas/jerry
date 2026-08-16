//! The real generator behind the five bundled non-Jerry-Dark theme files, and the parity tests
//! that keep them honest.
//!
//! ## Why these files are generated, not hand-written
//!
//! Before the theme system's rewrite, this app had exactly one hand-authored palette (Jerry Dark,
//! the ~270 `crate::theme::ColorToken` defaults) and five *derived* ones: each of the other bundled
//! themes was five swatches, and every one of the app's colours was computed from them on the fly,
//! per token, on every single `resolve()` call, by `crate::theme::derive_shift`'s HSL transform.
//!
//! The rewrite replaced that with real, literal, per-token theme files - which raised an obvious
//! requirement: the five bundled themes had to keep looking **exactly** as they did, not
//! approximately. Hand-transcribing ~270 derived hex values per theme by eyeballing that
//! transform's output would have been both enormous and untrustworthy. So they are generated
//! instead: [`generate_builtin_theme_toml`] runs the *same* real derivation over every registered
//! token once and writes the literal results into `assets/themes/*.toml`.
//!
//! The five swatch sets each theme was originally defined by are pinned here
//! ([`BUILTIN_THEME_SOURCES`]) as the real, transcribed inputs that derivation ran on - so the
//! generator can be re-run and its output compared against what is actually checked in
//! ([`tests::every_checked_in_builtin_theme_file_matches_the_generator`]), and so the *old*
//! mechanism's own output can be recomputed and compared token by token against what the new
//! mechanism really resolves
//! ([`tests::every_builtin_theme_resolves_exactly_what_the_old_derivation_produced`]) - the real
//! proof that this migration changed no colours.
//!
//! ## Regenerating
//!
//! ```text
//! JERRY_REGENERATE_THEMES=1 cargo test -p app --lib builtin_themes -- --nocapture
//! ```
//!
//! That writes every file in [`BUILTIN_THEME_SOURCES`] straight into `assets/themes/`. It is a
//! real, checked-in dev utility rather than a throwaway script because the same generator is what
//! the Themes page's "Generate from colour" action uses
//! (`crate::settings::render::AdeApp::generate_theme_from_seed_color` -> [`generated_theme_file`]),
//! so it has to keep working regardless.

use crate::settings::custom_theme::{CustomTheme, CustomThemeFile};
use crate::theme;

/// One bundled theme's real, pinned identity - the exact `name`/`subtitle` it has always had, the
/// file it lives in, and the five `[background, panel, green-ish, amber-ish, blue-ish]` swatches
/// it was originally defined by (transcribed verbatim from the pre-rewrite
/// `assets/themes/*.toml` files, which held exactly these five values and nothing else).
///
/// Those swatches are no longer what the app renders from - each theme's file now holds a full,
/// literal palette - but they remain load-bearing in two real ways: they are the input the
/// generator derives that palette from, and they are what each file carries forward as its
/// `preview`, so the Themes page's cards paint exactly what they always did.
pub struct BuiltinThemeSource {
    pub name: &'static str,
    pub subtitle: &'static str,
    pub file_name: &'static str,
    pub swatches: [u32; 5],
}

/// The six bundled themes, in the exact order `crate::settings::state::THEME_DEFS` lists them
/// (index `0` is Jerry Dark, the real default).
pub const BUILTIN_THEME_SOURCES: [BuiltinThemeSource; 6] = [
    BuiltinThemeSource {
        name: "Jerry Dark",
        subtitle: "default",
        file_name: "jerry-dark.toml",
        swatches: [0x0e0f11, 0x1a1e21, 0x5cb87f, 0xe2a336, 0x74ade8],
    },
    BuiltinThemeSource {
        name: "Jerry Dim",
        subtitle: "lower contrast",
        file_name: "jerry-dim.toml",
        swatches: [0x15181b, 0x20252a, 0x6ab97f, 0xd8a94a, 0x7f9ad4],
    },
    BuiltinThemeSource {
        name: "Slate",
        subtitle: "cool greys",
        file_name: "slate.toml",
        swatches: [0x0d1117, 0x161b22, 0x57a773, 0xc9a227, 0x6b9bd1],
    },
    BuiltinThemeSource {
        name: "Ember",
        subtitle: "warm",
        file_name: "ember.toml",
        swatches: [0x12100e, 0x1e1a16, 0x8fae6b, 0xd98b3a, 0xc4713f],
    },
    BuiltinThemeSource {
        name: "Moss",
        subtitle: "green-tinted",
        file_name: "moss.toml",
        swatches: [0x0f1310, 0x1a201b, 0x7fc79a, 0xc8b45a, 0x6f9bb5],
    },
    BuiltinThemeSource {
        name: "Paper",
        subtitle: "light \u{b7} beta",
        file_name: "paper.toml",
        swatches: [0xf4f1ea, 0xe4e0d6, 0x3f7a52, 0xa8752a, 0x3d6c9c],
    },
];

/// Jerry Dark's own swatches - the base every derivation is measured against (see
/// [`crate::theme::derive_shift`]).
pub fn jerry_dark_swatches() -> [u32; 5] {
    BUILTIN_THEME_SOURCES[0].swatches
}

/// Builds a real, complete, writable theme file from a derived palette - the one shared shape both
/// the built-in generator below and the Themes page's "Generate from colour" action produce, so
/// the two can never emit differently-structured files.
///
/// `base` is always `Some("Jerry Dark")` for anything generated: a generated file names every
/// token explicitly, so the base changes nothing about how it renders, but it makes the file's
/// intent obvious and means a hand-edit that *deletes* a line degrades to Jerry Dark's own value
/// for that token rather than to nothing.
pub fn generated_theme_file(
    name: &str,
    subtitle: &str,
    preview: [u32; 5],
    palette: Vec<(&'static str, gpui::Rgba)>,
) -> CustomThemeFile {
    CustomTheme {
        name: name.to_string(),
        subtitle: subtitle.to_string(),
        base: Some("Jerry Dark".to_string()),
        preview: Some(preview),
        overrides: palette.into_iter().collect(),
        source_path: None,
    }
    .to_file()
}

/// The real TOML text for one bundled theme's `assets/themes/*.toml` file, header comment
/// included.
///
/// Jerry Dark is the one real special case: it *is* the compiled default palette
/// (`crate::theme::ColorToken::default`), so its file names no colours at all - only its identity
/// and its card preview. Writing out 270 lines that restate the defaults would be redundant, and
/// worse, would mean two places to change if a default is ever retuned.
#[allow(clippy::expect_used)] // a generated theme file must be valid by construction
pub fn generate_builtin_theme_toml(source: &BuiltinThemeSource) -> String {
    let is_jerry_dark = source.swatches == jerry_dark_swatches();
    let file = if is_jerry_dark {
        CustomTheme {
            name: source.name.to_string(),
            subtitle: source.subtitle.to_string(),
            base: None,
            preview: Some(source.swatches),
            overrides: std::collections::HashMap::new(),
            source_path: None,
        }
        .to_file()
    } else {
        let shift = theme::derive_shift(jerry_dark_swatches(), source.swatches);
        generated_theme_file(
            source.name,
            source.subtitle,
            source.swatches,
            theme::derived_palette(shift),
        )
    };

    let header = if is_jerry_dark {
        "# Jerry Dark - this app's default theme, and the base every other theme inherits from.\n\
         #\n\
         # This file deliberately names no colours: every one of Jerry Dark's ~270 values is the\n\
         # compiled-in default of the matching token, so restating them here would just be a\n\
         # second copy to keep in sync. Copy any other bundled theme instead if you want a full\n\
         # palette to start from.\n\
         #\n\
         # Generated by `crate::settings::builtin_themes` - see that module's docs to regenerate."
            .to_string()
    } else {
        format!(
            "# {} - a bundled Jerry theme, and a worked example of the theme file format.\n\
             #\n\
             # GENERATED FILE. Every colour below was derived from Jerry Dark's own palette by the\n\
             # HSL transform in `crate::theme::derive_shift`, using this theme's five original\n\
             # swatches ({}), then written out literally - so it renders\n\
             # exactly as this theme always has while being an ordinary, hand-editable theme file\n\
             # with no special status.\n\
             #\n\
             # Regenerate with `crate::settings::builtin_themes` - see that module's docs.",
            source.name,
            source
                .swatches
                .iter()
                .map(|value| format!("#{value:06x}"))
                .collect::<Vec<_>>()
                .join(", "),
        )
    };
    crate::settings::theme_file_format::write_theme_toml(
        &file
            .validate_with_builtin_check(false)
            .expect("a generated theme file must be valid by construction")
            .to_file(),
        &header,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::custom_theme::parse_theme_file_str;
    use crate::settings::state::THEME_DEFS;

    /// The real, checked-in bytes of each bundled theme file, in [`BUILTIN_THEME_SOURCES`] order -
    /// embedded exactly the way `crate::settings::state::THEME_DEFS` embeds them, so these tests
    /// check the same bytes the app actually ships with.
    const CHECKED_IN: [&str; 6] = [
        include_str!("../../../../assets/themes/jerry-dark.toml"),
        include_str!("../../../../assets/themes/jerry-dim.toml"),
        include_str!("../../../../assets/themes/slate.toml"),
        include_str!("../../../../assets/themes/ember.toml"),
        include_str!("../../../../assets/themes/moss.toml"),
        include_str!("../../../../assets/themes/paper.toml"),
    ];

    /// The real regeneration utility (see this module's own docs) - a no-op unless
    /// `JERRY_REGENERATE_THEMES=1` is set, so an ordinary `cargo test` run never writes to the
    /// repository.
    #[test]
    fn regenerate_builtin_theme_files_when_asked() {
        if std::env::var("JERRY_REGENERATE_THEMES").as_deref() != Ok("1") {
            return;
        }
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/themes")
            .canonicalize()
            .expect("the real assets/themes directory must exist");
        for source in &BUILTIN_THEME_SOURCES {
            let path = assets.join(source.file_name);
            std::fs::write(&path, generate_builtin_theme_toml(source))
                .unwrap_or_else(|err| panic!("failed to write {}: {err}", path.display()));
            println!("wrote {}", path.display());
        }
    }

    /// The checked-in files really are this generator's own output - so a future change to the
    /// derivation, to the registry, or to the file writer can't silently leave the shipped themes
    /// stale.
    #[test]
    fn every_checked_in_builtin_theme_file_matches_the_generator() {
        for (source, checked_in) in BUILTIN_THEME_SOURCES.iter().zip(CHECKED_IN.iter()) {
            assert_eq!(
                generate_builtin_theme_toml(source),
                *checked_in,
                "assets/themes/{} is out of date - regenerate it (see this module's docs)",
                source.file_name
            );
        }
    }

    /// **The generator's own proof.** For every bundled theme and every single registered token,
    /// what the app resolves today (through the real, checked-in file, compiled the real way, with
    /// no derivation anywhere in the path) must equal what the generator produces right now -
    /// `derived_palette(derive_shift(jerry_dark, swatches))`.
    ///
    /// This used to compare against the *old* live HSL derivation instead, as the migration's
    /// bit-for-bit proof that turning derived palettes into files changed no colours. The theme
    /// redesign deliberately changed the derivation - to OKLCH, plus a real contrast-floor guard -
    /// so that comparison is no longer the right question and has been replaced by this one: the
    /// checked-in files must be exactly what today's generator makes, which is what stops a
    /// hand-edit or a stale file from surviving unnoticed.
    ///
    /// Compared at 8-bit-per-channel precision, which is the precision a theme file (and a
    /// rendered pixel) actually has.
    #[test]
    fn every_builtin_theme_resolves_exactly_what_the_generator_produces() {
        use crate::settings::custom_theme::compile_palette_by_name;

        for source in BUILTIN_THEME_SOURCES.iter().skip(1) {
            let palette = compile_palette_by_name(source.name, &[])
                .expect("a bundled theme must compile")
                .expect("a bundled non-Jerry-Dark theme has real overrides");
            let shift = theme::derive_shift(jerry_dark_swatches(), source.swatches);
            let generated: std::collections::HashMap<&str, gpui::Rgba> =
                theme::derived_palette(shift).into_iter().collect();

            for token in theme::all_tokens() {
                let expected = crate::settings::custom_theme::rgba_to_hex(
                    *generated
                        .get(token.key)
                        .expect("derived_palette covers every registered token"),
                );
                let actual = crate::settings::custom_theme::rgba_to_hex(
                    *palette
                        .get(token.key)
                        .unwrap_or_else(|| panic!("{} is missing from {}", token.key, source.name)),
                );
                assert_eq!(
                    expected, actual,
                    "{}: {} resolves to #{actual:06x}, but the generator produces #{expected:06x} \
                     - regenerate the bundled files (see this module's docs)",
                    source.name, token.key
                );
            }
        }
    }

    /// The redesign's own headline guarantee, checked on the real checked-in files rather than on
    /// the generator in memory: **every** bundled theme clears its syntax contrast floors. This is
    /// what `theme::enforce_syntax_contrast_floors` exists to make true, and it is stated here as
    /// well as in `theme::syntax_palette_tests` because this is the layer where a stale file
    /// (rather than a bad default) would break it.
    #[test]
    fn every_bundled_theme_file_clears_its_syntax_contrast_floors() {
        use crate::settings::custom_theme::compile_palette_by_name;

        for source in BUILTIN_THEME_SOURCES.iter() {
            let palette = compile_palette_by_name(source.name, &[])
                .expect("a bundled theme must compile")
                .unwrap_or_default();
            let resolve = |key: &str| {
                palette.get(key).copied().unwrap_or_else(|| {
                    theme::token_for_key(key)
                        .expect("a real registered key")
                        .default
                })
            };
            let background = resolve("surface.center");
            for token in theme::all_tokens() {
                let Some(floor) = theme::syntax_contrast_floor_for_test(token.key) else {
                    continue;
                };
                let ratio = theme::contrast_ratio(resolve(token.key), background);
                assert!(
                    ratio >= floor,
                    "{}: {} is {ratio:.2}:1 against the editor background, below its {floor}:1 \
                     floor",
                    source.name,
                    token.key
                );
            }
        }
    }

    /// Jerry Dark is the identity: with it selected, every token resolves to its own compiled
    /// default, exactly as it did before the rewrite (where index `0` short-circuited the
    /// derivation entirely).
    #[test]
    fn jerry_dark_is_still_a_real_identity_with_no_overrides_at_all() {
        let jerry_dark = parse_builtin(0);
        assert!(jerry_dark.overrides.is_empty());
        assert_eq!(jerry_dark.preview_swatches(), jerry_dark_swatches());
    }

    fn parse_builtin(index: usize) -> CustomTheme {
        let text = CHECKED_IN[index];
        let file = CustomThemeFile::from_toml_str(text).expect("must parse");
        file.validate_with_builtin_check(false)
            .expect("must validate")
    }

    /// Every bundled file is a real, ordinary theme file: it parses through the exact same
    /// user-facing parser, and (once renamed past the built-in collision check) validates.
    #[test]
    fn every_bundled_file_is_an_ordinary_theme_file_a_user_could_have_written() {
        for (index, source) in BUILTIN_THEME_SOURCES.iter().enumerate() {
            let renamed = CHECKED_IN[index].replace(
                &format!("name = \"{}\"", source.name),
                "name = \"Renamed For Test\"",
            );
            let theme = parse_theme_file_str(&renamed).unwrap_or_else(|err| {
                panic!("{} should parse as a user file: {err}", source.file_name)
            });
            assert_eq!(theme.name, "Renamed For Test");
        }
    }

    /// The generated palettes really are *different from each other* - a real guard against a
    /// generator bug that emitted the same (e.g. identity) palette six times and still passed
    /// every round-trip test.
    #[test]
    fn every_bundled_theme_is_a_genuinely_different_palette() {
        let windows: Vec<u32> = THEME_DEFS
            .iter()
            .map(|def| crate::settings::custom_theme::rgba_to_hex(def.theme.window_background()))
            .collect();
        let mut unique = windows.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            windows.len(),
            "two bundled themes share a window background: {windows:02x?}"
        );
    }

    /// GitHub issue #208, at the layer where a stale checked-in file (rather than a bad default)
    /// would break it: every bundled theme's terminal must really be its *own* terminal, and must
    /// really be readable.
    ///
    /// The readability floor is WCAG's own 4.5:1 "normal text" number, which is the right bar here
    /// (unlike `crate::settings::custom_theme::MIN_CONTRAST_PER_HUNDRED`, a deliberately far lower
    /// *validity* floor for arbitrary user-authored themes): these six files are this project's own
    /// generated output, so anything below the real bar is a generator bug to fix, not a theme to
    /// tolerate.
    #[test]
    fn every_bundled_theme_paints_its_own_readable_terminal() {
        use crate::settings::custom_theme::compile_palette_by_name;

        let mut backgrounds: Vec<u32> = Vec::new();
        for source in BUILTIN_THEME_SOURCES.iter() {
            let palette = compile_palette_by_name(source.name, &[])
                .expect("a bundled theme must compile")
                .unwrap_or_default();
            let resolve =
                |token: theme::ColorToken| palette.get(token.key).copied().unwrap_or(token.default);
            let background = resolve(theme::terminal::BACKGROUND);
            let foreground = resolve(theme::terminal::FOREGROUND);

            let ratio = theme::contrast_ratio(foreground, background);
            assert!(
                ratio >= 4.5,
                "{}: terminal.foreground is {ratio:.2}:1 against terminal.background - unstyled \
                 terminal output would be hard to read",
                source.name
            );

            // The pane is painted *into* `surface.pty`; a terminal fill that drifted away from it
            // would put the exact lighter-rectangle-inside-the-app back that issue #208 is about.
            assert_eq!(
                crate::settings::custom_theme::rgba_to_hex(background),
                crate::settings::custom_theme::rgba_to_hex(resolve(theme::surface::PTY)),
                "{}: the terminal fill has drifted off the surface it is painted into",
                source.name
            );

            backgrounds.push(crate::settings::custom_theme::rgba_to_hex(background));
        }

        let mut unique = backgrounds.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            backgrounds.len(),
            "two bundled themes paint the terminal the same colour, so switching between them \
             would not visibly change it: {backgrounds:06x?}"
        );
    }

    /// "Paper" is the one real light bundled theme - its generated `surface.window` must really be
    /// light, the same property the old derivation's lightness fit produced live.
    #[test]
    fn paper_really_generates_a_light_window_background_and_the_rest_stay_dark() {
        for def in THEME_DEFS.iter() {
            let is_light = theme::theme_is_light(def.theme.window_background());
            assert_eq!(
                is_light,
                def.name == "Paper",
                "{} should{} be a light theme",
                def.name,
                if def.name == "Paper" { "" } else { " not" }
            );
        }
    }
}
