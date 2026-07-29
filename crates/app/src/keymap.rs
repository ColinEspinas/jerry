//! One shared platform-style setting driving two related pieces of chrome
//! (`design_handoff_jerry_ade/CHANGELOG.md`'s 2026-07-29 entry, changes 1 and 2): which
//! title-bar variant `crate::root::title_bar` renders, and which glyphs a keybinding spec
//! string (e.g. `"mod+shift+K"`) resolves to for `crate::root::widgets`'s keycap renderers.
//! Deliberately GPUI-free, mirroring `crate::palette`/`crate::work_surface`'s own split: this
//! module only maps a platform choice onto plain strings, so that mapping is directly
//! unit-testable without a live GPUI window - turning a resolved combo into actual `gpui::Div`
//! keycap trees happens one layer up, in `crate::root::widgets`. [`resolve_keystroke`] is the one
//! real exception: it takes a real, plain-data `gpui::Keystroke` (no live window needed to
//! construct or inspect one) so the Settings › Keybindings page can resolve glyphs straight off
//! `crate::default_key_bindings`'s actual registered bindings - see that function's own docs for
//! why.
//!
//! ## Why one setting, not two
//!
//! The changelog leaves this an open judgment call: "your call whether these two are the same
//! setting or two independent ones, the design doesn't explicitly say". This module makes them
//! one, [`WindowControlsStyle`]: a user who overrides the title bar to look like Windows almost
//! certainly wants `Ctrl`/`Alt` keycaps too, not a title bar/keycap combination that doesn't
//! exist on any real OS. `design_handoff_jerry_ade/Jerry.dc.html`'s own reference
//! implementation agrees - its `tbStyle` local (`tbPick === 'macOS' ? 'macos' : tbPick ===
//! 'Windows' ? 'windows' : (plat === 'macos' ? 'macos' : 'windows')`) is derived from the exact
//! same `tbPick`/`plat()` pair its `km()` (the keymap-table lookup) already uses - one setting,
//! read twice, not two independently stored ones.
//!
//! ## This is a cosmetic preview, not a rebinding - and it can never be one
//!
//! A real, live-reproduced bug (see `crate::default_key_bindings`'s own docs) already proved
//! what happens when a rendered keycap promises a shortcut that doesn't actually work: real
//! silent failure, up to a stray keystroke leaking into whatever had focus. [`WindowControlsStyle`]
//! risks the exact same class of mismatch by design, not by accident, and it's worth being
//! explicit about why that's still the right call rather than quietly hoping nobody notices.
//!
//! `crate::default_key_bindings`'s three `"secondary-"` bindings are registered exactly once, at
//! real app startup (`crate::run`'s `cx.bind_keys` call), and `"secondary"` resolves to its
//! per-OS modifier via `cfg!(target_os = "macos")` - a **compile-time** fact. No runtime toggle,
//! including this one, can make the physical key that actually opens the palette on this
//! process's real OS ever be anything other than what it was compiled as. So: overriding
//! [`WindowControlsStyle`] to `MacosStyle` on a real Linux box makes every keycap in the app
//! render `⌘K` - but the key that actually opens the palette is still, and can only ever be,
//! Ctrl+K. That is a real, permanent mismatch between what's shown and what works, for as long
//! as the override is active.
//!
//! Three ways to resolve that tension were considered:
//! - **(a) Decouple entirely** - keep the title-bar override cosmetic-only and always resolve
//!   keycap glyphs from the real detected OS ([`detected_platform_is_macos`]), ignoring the
//!   override. This is the only option with zero risk of a misleading keycap, but it guts the
//!   feature: "preview macOS's look" would change the title bar and nothing else, which isn't
//!   what "preview" plausibly means to whoever reaches for this command, and would touch every
//!   one of the eight real call sites that currently read `window_controls_style.is_macos()`
//!   for keycap resolution.
//! - **(b) Make real key rebinding track the override live** - not viable: `cx.bind_keys` adds
//!   bindings to the app-global keymap, and nothing about a `Keystroke`'s `"secondary"` alias
//!   resolution is re-evaluated per-window or at runtime; it is resolved once, per `cfg!`, at
//!   compile time. There is no real GPUI API this app could call to make Cmd itself start
//!   working as a live app-level shortcut on Linux - only to bind the *literal* `"cmd-k"` alias
//!   permanently (independent of the override, always-on), which was rejected: it would
//!   silently resurrect exactly the original bug's confusion (two different keys opening the
//!   same thing, unlabeled) rather than fix it.
//! - **(c) Keep the coupling, document the honest limit** - chosen. Unlike the original
//!   bug, this mismatch only exists behind a deliberate, explicit, opt-in action (a
//!   command-palette entry whose own label is "macOS"/"Windows/Linux", not a default anyone
//!   hits by just launching the app), and its own docs (this section) and the palette entries'
//!   docs (`crate::root::palette_render`) now say plainly that switching this is a **preview of
//!   another platform's look**, not a promise that this session's real keys changed. That is a
//!   materially different risk profile from the original bug, which silently mismatched every
//!   Linux/Windows user's real, default, un-opted-into experience on every launch.
//!
//! If this ever grows real persistence (`CHANGELOG.md`'s R3 note below) or a way to actually
//! rebind live, revisit this decision rather than assuming it still holds.
//!
//! ## Real platform detection, verified
//!
//! [`std::env::consts::OS`] is real Rust `std`, not a GPUI/`alacritty_terminal`/`gix` API, so no
//! vendor/zed lookup applies - confirmed directly on this dev machine instead
//! (`rustc /tmp/os_check.rs -o /tmp/os_check && /tmp/os_check` printed `linux`), and documented
//! upstream (`std::env::consts::OS`) to be `"macos"`/`"windows"`/`"linux"` on those platforms
//! respectively - the three values [`detected_platform_is_macos`] and the Settings-page-to-be
//! (`design_handoff_jerry_ade/README.md`'s `Default environment`/`Window controls` rows) both
//! care about.
//!
//! ## Persisted (R3)
//!
//! [`WindowControlsStyle`] is now a real field of `crate::settings_store::Settings`
//! (`crate::settings_store::WindowSettings::controls`), loaded from and saved to
//! `~/.config/jerry/settings.toml` - see that module's docs and [`WindowControlsStyle`]'s own
//! "Now persisted (R3)" doc, above. `crate::root::palette_render`'s three "Window controls: …"
//! command-palette entries and the General settings page's `Window controls` choice row both
//! read/write `crate::root::AdeApp::window_controls_style`'s real, single, saved value - not
//! two independent copies.

/// Which platform's chrome (title-bar variant, keycap glyphs) should render right now.
/// `System` (the default) follows [`detected_platform_is_macos`]; the other two variants pin a
/// specific look regardless of what's actually running, so a developer (or a curious user) can
/// preview the other platform's chrome without leaving the app.
///
/// This is a rendering-only preview: it can never change which physical key really triggers
/// [`crate::default_key_bindings`]'s real, globally-bound shortcuts, which are fixed at compile
/// time by the real OS - see this module's own "This is a cosmetic preview, not a rebinding"
/// docs, above, for why that's an honest, deliberate limitation rather than an oversight.
///
/// ## Now persisted (R3)
///
/// This type's `#[derive(Serialize, Deserialize)]` and per-variant `#[serde(rename = ...)]`
/// (`"system"`/`"macos"`/`"windows"`, matching `design_handoff_jerry_ade/revision/Jerry.dc.html`'s
/// own `cfgSnippets.general` fixture: `controls = "system" # system · macos · windows`) back
/// `crate::settings_store::WindowSettings::controls` - the real, single source of truth this
/// module's own "Not yet persisted" section (below) used to say didn't exist yet. It does now:
/// `crate::root::AdeApp::window_controls_style` reads/writes `self.settings.window.controls`
/// directly, so the General settings page's `Window controls` choice row and the command
/// palette's three `Window controls: …` entries mutate the exact same real, saved field.
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
    /// The command palette's/settings-page-to-be's real label for this style.
    pub fn label(self) -> &'static str {
        match self {
            WindowControlsStyle::System => "System",
            WindowControlsStyle::MacosStyle => "macOS",
            WindowControlsStyle::WindowsLinuxStyle => "Windows/Linux",
        }
    }

    /// Whether this style resolves to macOS-style chrome right now. `System` defers to the
    /// real detected OS ([`detected_platform_is_macos`]); the other two variants are a pinned
    /// override, independent of what's actually running - `crate::root::AdeApp::
    /// render_title_bar` and every keycap-resolving render call site read this one method,
    /// never `std::env::consts::OS` directly, so the override always takes effect everywhere
    /// at once.
    pub fn is_macos(self) -> bool {
        match self {
            WindowControlsStyle::MacosStyle => true,
            WindowControlsStyle::WindowsLinuxStyle => false,
            WindowControlsStyle::System => detected_platform_is_macos(),
        }
    }
}

/// Real OS detection - see the module docs for how this was verified.
fn detected_platform_is_macos() -> bool {
    std::env::consts::OS == "macos"
}

/// The two-column glyph table `design_handoff_jerry_ade/README.md`'s "Keyboard affordances"
/// section describes: `mod alt ctrl shift enter esc tab bksp` on macOS vs. Windows/Linux.
/// Transcribed verbatim from `Jerry.dc.html`'s own `KEYMAPS` fixture (`macos: { mod: '⌘', alt:
/// '⌥', ctrl: '⌃', shift: '⇧', enter: '⏎', esc: 'esc', tab: '⇥', bksp: '⌫' }`, `other: { mod:
/// 'Ctrl', alt: 'Alt', ctrl: 'Ctrl', shift: 'Shift', enter: 'Enter', esc: 'Esc', tab: 'Tab',
/// bksp: 'Bksp' }`).
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
/// (`"N"`, `"K"`) or a decorative placeholder (`"1…8"`) passes straight through unresolved,
/// exactly like `Jerry.dc.html`'s own `combo(spec)`: `String(spec).split('+').map(p => k[p] ||
/// p)`.
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
/// (`crate::root::widgets::render_keycap_row`'s real input), never as a literal `⌘`/`⌥`/`⌃`/
/// `⇧`/`⏎`/`⇥`/`⌫` in calling code.
pub fn resolve_combo(spec: &str, macos: bool) -> Vec<String> {
    spec.split('+')
        .map(|token| resolve_token(token, macos))
        .collect()
}

/// Resolves a real, already-registered `gpui::Keystroke` (`crate::default_key_bindings`'s
/// actual, live `gpui::KeyBinding`s, not a hand-transcribed spec string) into the same real
/// per-platform glyphs [`resolve_combo`] produces - the Settings › Keybindings page's real input
/// (`crate::settings::keybinding_rows`), so its rows read directly off what's really bound and
/// can never drift the way a second, hand-copied `"mod+K"`-style list already had (a wrong
/// `context` label, an order that quietly stopped matching real registration order).
///
/// This is the one place in this module that takes a real `gpui::` type - every real, global
/// keybinding `crate::default_key_bindings` registers is authored with either no modifier at all
/// (`"f12"`) or exactly GPUI's own `"secondary-"` prefix (never a literal, independent `"cmd-"`/
/// `"ctrl-"`), which `gpui::Keystroke::parse` resolves to exactly one of `modifiers.platform`
/// (macOS) or `modifiers.control` (every other OS) at compile time - see
/// `crate::default_key_bindings`'s own docs. `Modifiers::secondary()` is GPUI's own real,
/// already-verified answer to "is this real, compiled modifier that same 'secondary' concept",
/// so folding it into this table's cross-platform `modifier` glyph (rather than the physical
/// `ctrl` glyph) reconstructs *this exact compiled binary's* real registered shortcut, not a
/// second guess - a literal, independent `ctrl`/`alt`/`shift` held alongside it (none of this
/// app's four real global bindings do, but a future one could) still renders as its own,
/// separate glyph.
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

/// `gpui::Keystroke::parse` always stores `key` lowercased (`vendor/zed/crates/gpui/src/
/// platform/keystroke.rs`'s own real `key_str.make_ascii_lowercase()`), unlike every
/// hand-authored spec string this module already renders (`"mod+N"`, `"F12"`) which is written
/// already-capitalized. [`resolve_token`] alone would silently start rendering real, derived keys
/// in lowercase (`"n"`, `"f12"`) even though every other keycap in this app is capitalized - this
/// wrapper applies the same capitalization a hand-authored spec already carries, but only to a
/// token [`resolve_token`] didn't itself recognize and resolve to a named glyph (an `enter`/`esc`/
/// `tab`/... token is already correctly-cased by its table entry, and must not be re-cased).
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
        // Exactly what `gpui::Keystroke::parse("secondary-n")` produces on a non-macOS build -
        // `modifiers.control == true`, `modifiers.platform == false` (see that function's own
        // docs, quoted in `crate::default_key_bindings`).
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

    // Note: `resolve_keystroke`'s `else if modifiers.control` branch (a real, independent `ctrl`
    // modifier held *without* `secondary`) is only reachable on a real macOS build, where
    // `Modifiers::secondary()` (`vendor/zed/crates/gpui/src/platform/keystroke.rs`) resolves to
    // `modifiers.platform`, not `modifiers.control` - on this Linux dev/test sandbox (this app's
    // only currently-supported, currently-verifiable platform - see
    // `crate::root::settings_widgets::open_settings_file`'s own docs), `secondary()` *is*
    // `modifiers.control`, so any keystroke with `control: true` already takes the `secondary`
    // branch and there is no real, constructible `Keystroke` value here that could reach the
    // `else if` at all. Not tested for the same honest reason `crate::keymap::WindowControlsStyle`'s
    // own docs give for its own `cfg!(target_os = "macos")`-gated behavior: real, compile-time-
    // correct code this sandbox cannot compile-and-run the other branch of.

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
