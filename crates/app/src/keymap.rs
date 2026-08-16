//! One shared platform-style setting driving two related pieces of chrome: which title-bar
//! variant `crate::title_bar` renders, and which glyphs a keybinding spec string (e.g.
//! `"mod+shift+K"`) resolves to for `crate::root::widgets`'s keycap renderers.
//!
//! Deliberately GPUI-free, mirroring `crate::palette::state`/`crate::work_surface::state`'s own split: this
//! module only maps a platform choice onto plain strings, so that mapping is directly
//! unit-testable without a live GPUI window - turning a resolved combo into `gpui::Div` keycap
//! trees happens one layer up, in `crate::root::widgets`. [`resolve_keystroke`] is the one real
//! exception: it takes a real, plain-data `gpui::Keystroke` (no live window needed) so the
//! Settings › Keybindings page can resolve glyphs straight off `crate::default_key_bindings`'s
//! actual registered bindings.
//!
//! ## One setting, not two
//!
//! [`WindowControlsStyle`] couples the title-bar look and the keycap glyphs: a user who
//! overrides the title bar to look like Windows almost certainly wants `Ctrl`/`Alt` keycaps
//! too, not a combination that doesn't exist on any real OS.
//!
//! ## This is a cosmetic preview, not a rebinding
//!
//! `crate::default_key_bindings`'s global bindings are registered exactly once, at real app
//! startup, and `"secondary"` resolves to its per-OS modifier via `cfg!(target_os = "macos")` -
//! a **compile-time** fact. No runtime toggle, including this one, can change which physical key
//! actually triggers a shortcut on this process's real OS. So overriding
//! [`WindowControlsStyle`] to `MacosStyle` on Linux renders `⌘P` everywhere, but the key that
//! actually opens the palette is still Ctrl+P - a real, permanent mismatch between what's shown
//! and what works while the override is active.
//!
//! This is accepted rather than fixed by decoupling entirely (which would gut the "preview
//! another platform's look" feature down to the title bar alone) or by trying to make Cmd
//! itself start working as a live shortcut on Linux (not possible - GPUI resolves `"secondary"`
//! once, at compile time, per `cfg!`). The mismatch only exists behind a deliberate, explicit,
//! opt-in command-palette action whose own label says "preview", not a promise that this
//! agent's keys changed - a materially different risk profile than the original bug below,
//! which silently mismatched every user's default, un-opted-into experience.
//!
//! ## Real platform detection
//!
//! [`std::env::consts::OS`] is real Rust `std`, documented to be `"macos"`/`"windows"`/`"linux"`
//! on those platforms - the three values [`detected_platform_is_macos`] cares about.
//!
//! ## Persisted (R3)
//!
//! [`WindowControlsStyle`] is a real field of `crate::settings::store::Settings`
//! (`WindowSettings::controls`), loaded from and saved to `~/.config/jerry/settings.toml`.
//! `crate::root::AdeApp::window_controls_style` reads/writes that field directly, so the General
//! settings page's `Window controls` row and the command palette's three `Window controls: …`
//! entries mutate the same saved value.

/// Which platform's chrome (title-bar variant, keycap glyphs) should render right now. `System`
/// (the default) follows [`detected_platform_is_macos`]; the other two variants pin a specific
/// look regardless of what's actually running, so a developer (or curious user) can preview the
/// other platform's chrome without leaving the app.
///
/// This is a rendering-only preview: it can never change which physical key really triggers
/// [`crate::default_key_bindings`]'s global shortcuts, which are fixed at compile time by the
/// real OS - see this module's own docs, above, for why that's a deliberate limitation.
///
/// `#[derive(Serialize, Deserialize)]` with per-variant `#[serde(rename = ...)]`
/// (`"system"`/`"macos"`/`"windows"`) backs `crate::settings::store::WindowSettings::controls` -
/// see this module's "Persisted (R3)" docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum WindowControlsStyle {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "macos")]
    MacosStyle,
    #[serde(rename = "windows")]
    WindowsLinuxStyle,
}

impl WindowControlsStyle {
    /// The command palette's/settings-page's label for this style.
    pub fn label(self) -> &'static str {
        match self {
            WindowControlsStyle::System => "System",
            WindowControlsStyle::MacosStyle => "macOS",
            WindowControlsStyle::WindowsLinuxStyle => "Windows/Linux",
        }
    }

    /// Whether this style resolves to macOS-style chrome right now. `System` defers to the
    /// real detected OS ([`detected_platform_is_macos`]); the other two variants are a pinned
    /// override. Every keycap-resolving render call site reads this method, never
    /// `std::env::consts::OS` directly, so the override takes effect everywhere at once.
    pub fn is_macos(self) -> bool {
        match self {
            WindowControlsStyle::MacosStyle => true,
            WindowControlsStyle::WindowsLinuxStyle => false,
            WindowControlsStyle::System => detected_platform_is_macos(),
        }
    }
}

/// Real OS detection - see the module docs for what values to expect.
fn detected_platform_is_macos() -> bool {
    std::env::consts::OS == "macos"
}

/// The two-column glyph table (`docs/design/vocabulary.md`): `mod alt ctrl shift enter esc tab
/// bksp` on macOS vs. Windows/Linux.
struct KeymapTable {
    modifier: &'static str,
    alt: &'static str,
    ctrl: &'static str,
    shift: &'static str,
    enter: &'static str,
    esc: &'static str,
    tab: &'static str,
    bksp: &'static str,
}

const MACOS_TABLE: KeymapTable = KeymapTable {
    modifier: "\u{2318}", // ⌘
    alt: "\u{2325}",      // ⌥
    ctrl: "\u{2303}",     // ⌃
    shift: "\u{21e7}",    // ⇧
    enter: "\u{23ce}",    // ⏎
    esc: "esc",
    tab: "\u{21e5}",  // ⇥
    bksp: "\u{232b}", // ⌫
};

const WINDOWS_LINUX_TABLE: KeymapTable = KeymapTable {
    modifier: "Ctrl",
    alt: "Alt",
    ctrl: "Ctrl",
    shift: "Shift",
    enter: "Enter",
    esc: "Esc",
    tab: "Tab",
    bksp: "Bksp",
};

/// Resolves one spec token (`"mod"`, `"shift"`, ...) to its platform glyph, or returns the
/// token unchanged if it isn't one of the eight recognized modifier/key names - a bare letter
/// (`"N"`, `"K"`) or a decorative placeholder (`"1…8"`) passes straight through unresolved.
pub fn resolve_token(token: &str, macos: bool) -> String {
    let table = if macos {
        &MACOS_TABLE
    } else {
        &WINDOWS_LINUX_TABLE
    };
    match token {
        "mod" => table.modifier,
        "alt" => table.alt,
        "ctrl" => table.ctrl,
        "shift" => table.shift,
        "enter" => table.enter,
        "esc" => table.esc,
        "tab" => table.tab,
        "bksp" => table.bksp,
        other => other,
    }
    .to_string()
}

/// Parses a spec string like `"mod+shift+K"` into its resolved parts (`["⌘", "⇧", "K"]` on
/// macOS, `["Ctrl", "Shift", "K"]` on Windows/Linux) - every keybinding hint in this app is
/// authored as one of these spec strings and rendered through this function
/// (`crate::root::widgets::render_keycap_row`), never as a literal glyph in calling code.
pub fn resolve_combo(spec: &str, macos: bool) -> Vec<String> {
    spec.split('+')
        .map(|token| resolve_token(token, macos))
        .collect()
}

/// Resolves a real, already-registered `gpui::Keystroke` (`crate::default_key_bindings`'s live
/// `gpui::KeyBinding`s, not a hand-transcribed spec string) into the same per-platform glyphs
/// [`resolve_combo`] produces - the Settings › Keybindings page's input
/// (`crate::settings::state::keybinding_rows`), so its rows read directly off what's really bound and
/// can't drift the way a hand-copied list once did (R3: a wrong `context` label, a stale order).
///
/// `modifiers.secondary()` maps to the cross-platform `modifier` glyph. A literal `control` held
/// without `secondary` falls back to the physical `ctrl` glyph instead - reachable on macOS
/// (where `secondary()` tracks `platform`, not `control`) for a binding like
/// `crate::default_key_bindings`'s `"ctrl-shift-t"`, which is a real, literal Ctrl on every OS.
pub fn resolve_keystroke(keystroke: &gpui::Keystroke, macos: bool) -> Vec<String> {
    let table = if macos {
        &MACOS_TABLE
    } else {
        &WINDOWS_LINUX_TABLE
    };
    let modifiers = &keystroke.modifiers;
    let mut parts = Vec::new();
    if modifiers.secondary() {
        parts.push(table.modifier.to_string());
    } else if modifiers.control {
        parts.push(table.ctrl.to_string());
    }
    if modifiers.alt {
        parts.push(table.alt.to_string());
    }
    if modifiers.shift {
        parts.push(table.shift.to_string());
    }
    parts.push(resolve_keystroke_key(&keystroke.key, macos));
    parts
}

/// `gpui::Keystroke::parse` always stores `key` lowercased
/// (`vendor/zed/crates/gpui/src/platform/keystroke.rs`), unlike every hand-authored spec string
/// this module renders (`"mod+N"`, `"F12"`), which is already capitalized. This wrapper applies
/// the same capitalization to a token [`resolve_token`] didn't itself resolve to a named glyph -
/// an `enter`/`esc`/`tab`/... token is already correctly-cased by its table entry and must not
/// be re-cased.
fn resolve_keystroke_key(key: &str, macos: bool) -> String {
    let resolved = resolve_token(key, macos);
    if resolved == key {
        resolved.to_uppercase()
    } else {
        resolved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_combo_splits_on_plus_and_resolves_each_recognized_token() {
        assert_eq!(
            resolve_combo("mod+shift+K", true),
            vec!["\u{2318}", "\u{21e7}", "K"]
        );
        assert_eq!(
            resolve_combo("mod+shift+K", false),
            vec!["Ctrl", "Shift", "K"]
        );
    }

    #[test]
    fn unrecognized_tokens_pass_through_unchanged_on_both_platforms() {
        assert_eq!(resolve_combo("mod+N", true), vec!["\u{2318}", "N"]);
        assert_eq!(resolve_combo("mod+N", false), vec!["Ctrl", "N"]);
        assert_eq!(resolve_combo("1\u{2026}8", true), vec!["1\u{2026}8"]);
        assert_eq!(resolve_combo("1\u{2026}8", false), vec!["1\u{2026}8"]);
    }

    #[test]
    fn a_single_token_spec_resolves_to_one_part() {
        assert_eq!(resolve_combo("esc", true), vec!["esc"]);
        assert_eq!(resolve_combo("esc", false), vec!["Esc"]);
    }

    #[test]
    fn every_table_entry_resolves_to_its_macos_glyph() {
        for (token, glyph) in [
            ("mod", "\u{2318}"),
            ("alt", "\u{2325}"),
            ("ctrl", "\u{2303}"),
            ("shift", "\u{21e7}"),
            ("enter", "\u{23ce}"),
            ("esc", "esc"),
            ("tab", "\u{21e5}"),
            ("bksp", "\u{232b}"),
        ] {
            assert_eq!(resolve_token(token, true), glyph, "token {token}");
        }
    }

    #[test]
    fn every_table_entry_resolves_to_its_windows_linux_word_on_the_other_platform() {
        for (token, word) in [
            ("mod", "Ctrl"),
            ("alt", "Alt"),
            ("ctrl", "Ctrl"),
            ("shift", "Shift"),
            ("enter", "Enter"),
            ("esc", "Esc"),
            ("tab", "Tab"),
            ("bksp", "Bksp"),
        ] {
            assert_eq!(resolve_token(token, false), word, "token {token}");
        }
    }

    #[test]
    fn window_controls_style_system_defers_to_the_real_detected_platform() {
        assert_eq!(
            WindowControlsStyle::System.is_macos(),
            detected_platform_is_macos()
        );
    }

    #[test]
    fn window_controls_style_pinned_variants_ignore_the_real_platform_and_override_it() {
        assert!(WindowControlsStyle::MacosStyle.is_macos());
        assert!(!WindowControlsStyle::WindowsLinuxStyle.is_macos());
    }

    #[test]
    fn the_override_changes_combo_resolution_the_same_way_a_real_platform_switch_would() {
        assert_eq!(
            resolve_combo("mod+N", WindowControlsStyle::MacosStyle.is_macos()),
            vec!["\u{2318}", "N"]
        );
        assert_eq!(
            resolve_combo("mod+N", WindowControlsStyle::WindowsLinuxStyle.is_macos()),
            vec!["Ctrl", "N"]
        );
    }

    #[test]
    fn default_window_controls_style_is_system() {
        assert_eq!(WindowControlsStyle::default(), WindowControlsStyle::System);
    }

    #[test]
    fn resolve_keystroke_renders_the_secondary_modifier_as_the_cross_platform_mod_glyph() {
        // What `gpui::Keystroke::parse("secondary-n")` produces on a non-macOS build -
        // `modifiers.control == true`, `modifiers.platform == false`.
        let keystroke = gpui::Keystroke {
            modifiers: gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            key: "n".to_string(),
            key_char: None,
        };
        assert_eq!(
            resolve_keystroke(&keystroke, false),
            vec!["Ctrl".to_string(), "N".to_string()]
        );
        assert_eq!(
            resolve_keystroke(&keystroke, true),
            vec!["\u{2318}".to_string(), "N".to_string()]
        );
    }

    #[test]
    fn resolve_keystroke_with_no_modifiers_capitalizes_an_unrecognized_key_like_a_hand_authored_spec(
    ) {
        // `gpui::Keystroke::parse("f12")` stores `key == "f12"` (lowercased) - real, live-bound
        // `crate::default_key_bindings`'s `f12` entry - but every hand-authored spec string this
        // module renders capitalizes bare keys (`"F12"`), so a derived key must match.
        let keystroke = gpui::Keystroke {
            modifiers: gpui::Modifiers::default(),
            key: "f12".to_string(),
            key_char: None,
        };
        assert_eq!(
            resolve_keystroke(&keystroke, false),
            vec!["F12".to_string()]
        );
    }

    // `resolve_keystroke`'s `else if modifiers.control` branch (a literal `ctrl` held *without*
    // `secondary`) is only reachable on a real macOS build: on this Linux dev/test sandbox,
    // `Modifiers::secondary()` *is* `modifiers.control`, so any keystroke with `control: true`
    // already takes the `secondary` branch first. Not tested here for the same reason
    // `WindowControlsStyle`'s own docs give for its `cfg!(target_os = "macos")`-gated behavior:
    // real, compile-time-correct code this sandbox cannot compile-and-run the other branch of.

    #[test]
    fn labels_are_the_real_command_palette_display_strings() {
        assert_eq!(WindowControlsStyle::System.label(), "System");
        assert_eq!(WindowControlsStyle::MacosStyle.label(), "macOS");
        assert_eq!(
            WindowControlsStyle::WindowsLinuxStyle.label(),
            "Windows/Linux"
        );
    }
}
