//! A serde-shaped theme file a user could populate, matching `crates/jerry-app/src/theme.rs`'s
//! own "a theme file's own text keys" convention: every colour is a `#rrggbb` (or `rrggbb`) hex
//! string. [`ThemeSchema::built_in`] is the built-in [`Theme`] expressed in this same schema, so
//! "the shipped default" and "a user override" are validated by exactly one code path rather
//! than two that could drift apart.
//!
//! This is deliberately a *separate*, smaller schema from `jerry-app`'s own ~270-key theme-file
//! format (`crate::settings::custom_theme` in that crate) - this one only ever populates the
//! handful of semantic tokens [`crate::theme::Colors`] defines, and knows nothing about
//! `jerry-app`'s settings store or file system (see `docs/architecture/decisions.md` §27).

use serde::{Deserialize, Serialize};

use crate::theme::{Colors, StatusColors, Theme};

/// One malformed or unrecognised theme file, with enough detail to show the user exactly which
/// field is wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    #[error("could not parse theme file: {0}")]
    Parse(String),
    #[error("theme field `{field}` is not a valid `#rrggbb` colour: `{value}`")]
    InvalidColor { field: &'static str, value: String },
}

/// The colour half of [`ThemeSchema`] - one `#rrggbb` string per [`Colors`] field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorSchema {
    pub surface: String,
    pub surface_raised: String,
    pub surface_hover: String,
    pub surface_selected: String,
    pub border: String,
    pub border_disabled: String,
    pub text: String,
    pub text_muted: String,
    pub text_disabled: String,
    pub text_on_accent: String,
    pub accent: String,
    pub accent_hover: String,
    pub status_ok: String,
    pub status_ok_bg: String,
    pub status_warn: String,
    pub status_warn_bg: String,
    pub status_fail: String,
    pub status_fail_bg: String,
    pub status_info: String,
    pub status_info_bg: String,
}

/// A theme file, deserialized but not yet validated - [`Self::validate`] is the one place a
/// `String` becomes a real [`Theme`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeSchema {
    pub colors: ColorSchema,
}

/// Parses `"#rrggbb"` or `"rrggbb"` into an opaque colour - the same format
/// `crates/jerry-app/src/theme.rs`'s own theme files already use.
fn parse_hex_color(field: &'static str, value: &str) -> Result<gpui::Hsla, SchemaError> {
    let digits = value.strip_prefix('#').unwrap_or(value);
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SchemaError::InvalidColor {
            field,
            value: value.to_string(),
        });
    }
    let bits = u32::from_str_radix(digits, 16).map_err(|_| SchemaError::InvalidColor {
        field,
        value: value.to_string(),
    })?;
    let rgba = gpui::Rgba {
        r: ((bits >> 16) & 0xff) as f32 / 255.0,
        g: ((bits >> 8) & 0xff) as f32 / 255.0,
        b: (bits & 0xff) as f32 / 255.0,
        a: 1.0,
    };
    Ok(rgba.into())
}

impl ThemeSchema {
    /// The built-in ([`Theme::default`]) theme, expressed in this schema - the default a user
    /// theme file overrides fields of, and the fixture [`Self::validate`]'s own tests compile
    /// straight back into an identical [`Theme`].
    pub fn built_in() -> Self {
        let theme = Theme::default();
        let hex = |color: gpui::Hsla| -> String {
            let rgba: gpui::Rgba = color.into();
            format!(
                "#{:02x}{:02x}{:02x}",
                (rgba.r * 255.0).round() as u8,
                (rgba.g * 255.0).round() as u8,
                (rgba.b * 255.0).round() as u8,
            )
        };
        ThemeSchema {
            colors: ColorSchema {
                surface: hex(theme.colors.surface),
                surface_raised: hex(theme.colors.surface_raised),
                surface_hover: hex(theme.colors.surface_hover),
                surface_selected: hex(theme.colors.surface_selected),
                border: hex(theme.colors.border),
                border_disabled: hex(theme.colors.border_disabled),
                text: hex(theme.colors.text),
                text_muted: hex(theme.colors.text_muted),
                text_disabled: hex(theme.colors.text_disabled),
                text_on_accent: hex(theme.colors.text_on_accent),
                accent: hex(theme.colors.accent),
                accent_hover: hex(theme.colors.accent_hover),
                status_ok: hex(theme.colors.status.ok),
                status_ok_bg: hex(theme.colors.status.ok_bg),
                status_warn: hex(theme.colors.status.warn),
                status_warn_bg: hex(theme.colors.status.warn_bg),
                status_fail: hex(theme.colors.status.fail),
                status_fail_bg: hex(theme.colors.status.fail_bg),
                status_info: hex(theme.colors.status.info),
                status_info_bg: hex(theme.colors.status.info_bg),
            },
        }
    }

    /// Parses every colour field and builds a real [`Theme`] - the non-colour tiers
    /// ([`crate::theme::Spacing`]/[`crate::theme::Radius`]/[`crate::theme::Motion`]/
    /// [`crate::theme::Dimensions`]) are not user-themeable yet, so this always carries
    /// [`Theme::default`]'s own values for those.
    pub fn validate(&self) -> Result<Theme, SchemaError> {
        let c = &self.colors;
        let colors = Colors {
            surface: parse_hex_color("colors.surface", &c.surface)?,
            surface_raised: parse_hex_color("colors.surface_raised", &c.surface_raised)?,
            surface_hover: parse_hex_color("colors.surface_hover", &c.surface_hover)?,
            surface_selected: parse_hex_color("colors.surface_selected", &c.surface_selected)?,
            border: parse_hex_color("colors.border", &c.border)?,
            border_disabled: parse_hex_color("colors.border_disabled", &c.border_disabled)?,
            text: parse_hex_color("colors.text", &c.text)?,
            text_muted: parse_hex_color("colors.text_muted", &c.text_muted)?,
            text_disabled: parse_hex_color("colors.text_disabled", &c.text_disabled)?,
            text_on_accent: parse_hex_color("colors.text_on_accent", &c.text_on_accent)?,
            accent: parse_hex_color("colors.accent", &c.accent)?,
            accent_hover: parse_hex_color("colors.accent_hover", &c.accent_hover)?,
            status: StatusColors {
                ok: parse_hex_color("colors.status_ok", &c.status_ok)?,
                ok_bg: parse_hex_color("colors.status_ok_bg", &c.status_ok_bg)?,
                warn: parse_hex_color("colors.status_warn", &c.status_warn)?,
                warn_bg: parse_hex_color("colors.status_warn_bg", &c.status_warn_bg)?,
                fail: parse_hex_color("colors.status_fail", &c.status_fail)?,
                fail_bg: parse_hex_color("colors.status_fail_bg", &c.status_fail_bg)?,
                info: parse_hex_color("colors.status_info", &c.status_info)?,
                info_bg: parse_hex_color("colors.status_info_bg", &c.status_info_bg)?,
            },
        };
        Ok(Theme {
            colors,
            ..Theme::default()
        })
    }
}

/// Parses a TOML theme file into a [`ThemeSchema`] - `toml::de::Error`'s own `Display` already
/// names the offending line/key, which [`SchemaError::Parse`] just forwards.
pub fn parse_toml(text: &str) -> Result<ThemeSchema, SchemaError> {
    toml::from_str(text).map_err(|err| SchemaError::Parse(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{parse_toml, SchemaError, ThemeSchema};
    use crate::theme::Theme;

    #[test]
    fn the_built_in_schema_validates_back_to_the_built_in_theme() {
        let schema = ThemeSchema::built_in();
        let theme = schema.validate().expect("built-in schema must validate");
        assert_eq!(theme, Theme::default());
    }

    #[test]
    fn a_real_toml_file_round_trips_through_parse_and_validate() {
        let text = toml::to_string(&ThemeSchema::built_in()).expect("serialize built-in schema");
        let schema = parse_toml(&text).expect("parse a real generated theme file");
        assert_eq!(schema, ThemeSchema::built_in());
        assert_eq!(
            schema.validate().expect("validate a real theme file"),
            Theme::default()
        );
    }

    #[test]
    fn an_invalid_colour_is_a_clear_field_level_error() {
        let mut schema = ThemeSchema::built_in();
        schema.colors.accent = "not-a-colour".to_string();
        let error = schema.validate().expect_err("bad hex must fail validation");
        assert_eq!(
            error,
            SchemaError::InvalidColor {
                field: "colors.accent",
                value: "not-a-colour".to_string(),
            }
        );
    }

    #[test]
    fn a_hash_prefixed_or_bare_hex_string_both_parse_the_same_colour() {
        let mut with_hash = ThemeSchema::built_in();
        with_hash.colors.accent = "#74ADE8".to_string();
        let mut without_hash = ThemeSchema::built_in();
        without_hash.colors.accent = "74ADE8".to_string();

        assert_eq!(
            with_hash.validate().expect("hash-prefixed hex"),
            without_hash.validate().expect("bare hex")
        );
    }

    #[test]
    fn an_unknown_field_is_a_clear_parse_error_not_a_silent_ignore() {
        // A real, fully-populated document with exactly one extra key spliced into its one
        // `[colors]` table - so the failure this asserts is `deny_unknown_fields` rejecting
        // `nonexistent_field`, not an unrelated malformed-document error.
        let document = toml::to_string(&ThemeSchema::built_in())
            .expect("serialize a real, fully-populated schema")
            .replace("[colors]\n", "[colors]\nnonexistent_field = \"#000000\"\n");
        let error = parse_toml(&document).expect_err("unknown field must be rejected");
        assert!(matches!(error, SchemaError::Parse(_)));
    }
}
