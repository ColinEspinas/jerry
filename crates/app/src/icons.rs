//! The app's shipped icon set: real [Phosphor Icons](https://phosphoricons.com) SVGs (MIT),
//! vendored under `assets/icons/`, plus the one render helper every consuming surface draws them
//! through (GitHub issue #282, `design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §8).
//!
//! ## Why an asset, not another hand-drawn shape
//!
//! Every "icon" in the mock is absolutely-positioned 1px `div`s, and §8 records what that cost:
//! "mismatched optical sizes in a row, two glyphs from one family a divider apart, marks that read
//! as nothing at 17px". The deciding argument for Phosphor is this app's own build target -
//! Phosphor ships raw SVGs and GPUI renders SVG natively, so each icon is a named asset
//! (`assets/icons/<name>.svg` + `gpui::svg()`) instead of geometry someone has to reinterpret as
//! paths.
//!
//! ## Scope: actions and views only
//!
//! §8's "Stays hand-drawn" list is Jerry's own vocabulary and is deliberately **not** touched by
//! this module: agent tint chips, status dots, file-extension chips, diff gutter marks, and the
//! graph's lanes and merge elbows. [`Icon`] is §8's eleven mapped slots plus the one slot §4u
//! names outside that table (the overflow menu's Settings row, GitHub issue #290), no more.
//!
//! ## Sizing: one optical box per row, never one size per icon
//!
//! `REVISION-2026-08-14.md` §7 rule 7, verbatim: "A row of icons needs one shared optical box, not
//! one size per icon." That is why the only way to draw an icon here is through an [`IconRow`]:
//! a row is constructed once with one [`IconSize`], and every [`IconRow::draw`] call on it uses
//! that same box. A lone icon (the rail footer's prune button) is simply a row of one.
//!
//! Every vendored Phosphor file is authored on the same `0 0 256 256` canvas (asserted by
//! [`tests::every_vendored_icon_is_a_real_phosphor_svg_on_the_shared_256_canvas`]), so drawing two
//! of them into equal boxes really does give them equal optical weight - the property the
//! hand-drawn shapes could not hold.
//!
//! ## Weight: bold, because every named size is below 20px
//!
//! §8's rule, verbatim: "**Weight:** `bold` at 15-17px (`regular`'s 1.5px stroke reads thin
//! against `#5e646a`); `regular` only at 20px+." [`weight_for_size`] is that rule as real code and
//! [`VENDORED_WEIGHT`] is what `assets/icons/` actually holds; a test pairs them for every
//! [`IconSize`], so adding a 20px+ size fails the test run until the matching `regular` files are
//! vendored too, rather than silently drawing a bold glyph where the design asks for a regular one.
//!
//! ## Tinting
//!
//! GPUI rasterises an SVG to an **alpha mask** and paints it as a `MonochromeSprite` tinted with
//! the element's own `text.color` (`vendor/zed/crates/gpui/src/elements/svg.rs:117` zips the path
//! with `style.text.color`; `window.rs`'s `paint_svg` keeps only `p.alpha()` from the rasterised
//! pixmap and colours the sprite). Two real consequences this module is built around:
//!
//! 1. A shipped icon tints like text, exactly as §8 wants - [`IconRow::draw`] takes the caller's
//!    own [`ColorToken`], so an icon picks up the live theme like every other painted thing here.
//! 2. An SVG element with **no** text colour paints *nothing at all* - `zip` yields `None` and
//!    `paint_svg` is never called. "Untinted" is not "keeps its own colours" in GPUI; it is
//!    "invisible". So [`IconRow::draw`] tints its icon-pack override too (see below) rather than
//!    shipping a branch that is guaranteed to draw an empty box.
//!
//! ## User icon packs still override, by name
//!
//! `crate::icon_pack::resolve_icon` is unchanged and still wins: a pack that really contains
//! `<name>.svg` for one of these names replaces the shipped file for that slot
//! ([`resolve_source`]). The pack's own icons are looked up by the same design-handoff names this
//! module uses (`folder`, `trash`, ...), so a pack author needs nothing but the name.
//!
//! Note this module deliberately does not change `crate::root::widgets`'
//! `render_agent_chip_icon`, the one older icon-pack call site - it is a separate,
//! already-shipped surface with a real difference from this one: a shipped Phosphor icon is
//! deliberately monochrome and *should* pick up a theme tint (point 1 above), while an
//! agent-chip pack icon is a full-color, user-supplied brand image that must keep its own
//! colors. GitHub issue #309 fixed that older call site's own version of point 2 above (it drew
//! empty boxes for the same "no text color" reason) by switching it to `gpui::img()`, which
//! doesn't consult `style.text.color` at all - not by adding a tint, which would have been wrong
//! for a full-color logo.

use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{px, InteractiveElement, Pixels, Styled, Svg};

use crate::icon_pack;
use crate::settings::store::IconPackSettings;
use crate::theme::ColorToken;

/// `(asset path, embedded bytes)` for every vendored Phosphor file - the real, complete contents
/// of `assets/icons/`, embedded with `include_bytes!` so a built binary carries them and never
/// reads that directory at runtime (exactly how `crate::fonts::FONT_FILES` handles the bundled
/// `.ttf`s). `crate::fonts::Assets` serves these through `gpui::AssetSource`, which is what
/// `gpui::svg().path(..)` loads from.
///
/// Order matches [`Icon::ALL`]; [`tests::icon_files_and_the_icon_enum_are_the_same_set`] holds
/// the two honestly in sync.
pub const ICON_FILES: &[(&str, &[u8])] = &[
    (
        "icons/arrows-left-right.svg",
        include_bytes!("../../../assets/icons/arrows-left-right.svg"),
    ),
    (
        "icons/caret-down.svg",
        include_bytes!("../../../assets/icons/caret-down.svg"),
    ),
    (
        "icons/clock-counter-clockwise.svg",
        include_bytes!("../../../assets/icons/clock-counter-clockwise.svg"),
    ),
    (
        "icons/folder.svg",
        include_bytes!("../../../assets/icons/folder.svg"),
    ),
    (
        "icons/funnel.svg",
        include_bytes!("../../../assets/icons/funnel.svg"),
    ),
    (
        "icons/git-branch.svg",
        include_bytes!("../../../assets/icons/git-branch.svg"),
    ),
    (
        "icons/magnifying-glass.svg",
        include_bytes!("../../../assets/icons/magnifying-glass.svg"),
    ),
    (
        "icons/sliders-horizontal.svg",
        include_bytes!("../../../assets/icons/sliders-horizontal.svg"),
    ),
    (
        "icons/terminal-window.svg",
        include_bytes!("../../../assets/icons/terminal-window.svg"),
    ),
    (
        "icons/trash.svg",
        include_bytes!("../../../assets/icons/trash.svg"),
    ),
    (
        "icons/tree-structure.svg",
        include_bytes!("../../../assets/icons/tree-structure.svg"),
    ),
    (
        "icons/warning.svg",
        include_bytes!("../../../assets/icons/warning.svg"),
    ),
];

/// One shipped Phosphor glyph. `REVISION-2026-08-14.md` §8's mapping table, plus the overflow
/// menu's Settings glyph §4u names (GitHub issue #290) - and no speculative extras: an icon
/// nothing draws is an affordance with no behaviour behind it, which §7 rule 1 rules out ("Ship
/// the affordance with the behaviour, or ship neither").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    /// Search panel's count row: replace.
    ArrowsLeftRight,
    /// Search panel's count row: fold-all.
    CaretDown,
    /// Sidebar strip: history.
    ClockCounterClockwise,
    /// Right-panel tab: Files.
    Folder,
    /// Search panel's count row: path filter.
    Funnel,
    /// Right-panel tab: Changes.
    GitBranch,
    /// Right-panel tab: Search.
    MagnifyingGlass,
    /// The `⋯` overflow menu: Settings (GitHub issue #290). `STAGE-A-CHANGELOG.md` §4u:
    /// History and Settings keep "the glyphs they had in the strip (clock, sliders) so the move
    /// out of the strip does not cost their recognisability".
    SlidersHorizontal,
    /// Work-surface tab strip: terminal.
    TerminalWindow,
    /// Rail footer: prune merged worktrees.
    Trash,
    /// Sidebar strip: worktrees.
    TreeStructure,
    /// Sidebar strip: problems.
    Warning,
}

impl Icon {
    /// Every shipped icon, in [`ICON_FILES`] order.
    pub const ALL: &'static [Icon] = &[
        Icon::ArrowsLeftRight,
        Icon::CaretDown,
        Icon::ClockCounterClockwise,
        Icon::Folder,
        Icon::Funnel,
        Icon::GitBranch,
        Icon::MagnifyingGlass,
        Icon::SlidersHorizontal,
        Icon::TerminalWindow,
        Icon::Trash,
        Icon::TreeStructure,
        Icon::Warning,
    ];

    /// This icon's stable name - Phosphor's own name for the glyph, which is also the name
    /// `REVISION-2026-08-14.md` §8's table uses and the name a user icon pack overrides it by
    /// (`crate::icon_pack::resolve_icon` looks for `<name>.svg`).
    pub const fn name(self) -> &'static str {
        match self {
            Icon::ArrowsLeftRight => "arrows-left-right",
            Icon::CaretDown => "caret-down",
            Icon::ClockCounterClockwise => "clock-counter-clockwise",
            Icon::Folder => "folder",
            Icon::Funnel => "funnel",
            Icon::GitBranch => "git-branch",
            Icon::MagnifyingGlass => "magnifying-glass",
            Icon::SlidersHorizontal => "sliders-horizontal",
            Icon::TerminalWindow => "terminal-window",
            Icon::Trash => "trash",
            Icon::TreeStructure => "tree-structure",
            Icon::Warning => "warning",
        }
    }

    /// The `gpui::AssetSource` path of this icon's shipped file - what `gpui::svg().path(..)`
    /// loads, and a real key in [`ICON_FILES`].
    pub const fn asset_path(self) -> &'static str {
        match self {
            Icon::ArrowsLeftRight => "icons/arrows-left-right.svg",
            Icon::CaretDown => "icons/caret-down.svg",
            Icon::ClockCounterClockwise => "icons/clock-counter-clockwise.svg",
            Icon::Folder => "icons/folder.svg",
            Icon::Funnel => "icons/funnel.svg",
            Icon::GitBranch => "icons/git-branch.svg",
            Icon::MagnifyingGlass => "icons/magnifying-glass.svg",
            Icon::SlidersHorizontal => "icons/sliders-horizontal.svg",
            Icon::TerminalWindow => "icons/terminal-window.svg",
            Icon::Trash => "icons/trash.svg",
            Icon::TreeStructure => "icons/tree-structure.svg",
            Icon::Warning => "icons/warning.svg",
        }
    }
}

/// A named optical box: the one square every icon in a given row is drawn inside. Each variant is
/// a real measurement off the reviewed mock (`design_handoff_jerry_ade/revision 5/Jerry.dc.html`),
/// not a round number picked here - the surfaces these serve are being rebuilt against that file.
///
/// Phosphor's canvas is square, so a box named here is square too. Where the mock's hand-drawn
/// stand-in was slightly non-square (the segmented tabs' stated 11x10), the square box is the
/// larger dimension, which is also what the mock's own markup really occupies - see
/// [`IconSize::PanelTab`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconSize {
    /// 11px - the right panel's segmented Files/Search/Changes tabs. `STAGE-A-CHANGELOG.md` §4w:
    /// "All three are drawn inside one 11x10 optical box at x 7-18, y 4-14 inside the 26x19
    /// button", after "the first cut drew each at whatever size suited it - folder 12x8,
    /// magnifier 8x8, diff 7x7 - so three icons on one row had three weights". The mock's own
    /// shapes actually span y 3.5-14.5, i.e. 11x11; 11 is the box both readings agree on.
    PanelTab,
    /// 13px - a row of the app's shared menu (`crate::menu`). `Jerry.dc.html`'s own context-menu
    /// row draws its leading glyph in a `width:13px;height:13px` box.
    MenuRow,
    /// 14px - the work-surface tab strip's per-tab chip (`Jerry.dc.html`:
    /// `width:14px;height:14px`), shared by every tab kind on that strip, terminal included.
    TabChip,
    /// 15px - the sidebar strip's buttons (`Jerry.dc.html`: each strip glyph sits in a
    /// `position:relative;width:15px;height:15px` wrapper inside a 38px-wide cell).
    Strip,
    /// 17px - the icon buttons: the search panel's count row (replace / filter / fold-all) and
    /// the rail footer's prune button, all `width:17px;height:17px` in `Jerry.dc.html`.
    /// §4w records fold-all being lifted 15 -> 17 for exactly the reason rule 7 states.
    Control,
}

impl IconSize {
    /// Every named size.
    pub const ALL: &'static [IconSize] = &[
        IconSize::PanelTab,
        IconSize::MenuRow,
        IconSize::TabChip,
        IconSize::Strip,
        IconSize::Control,
    ];

    /// The square this size's icons are drawn inside - the whole of "one shared optical box".
    pub const fn box_size(self) -> Pixels {
        match self {
            IconSize::PanelTab => px(11.0),
            IconSize::MenuRow => px(13.0),
            IconSize::TabChip => px(14.0),
            IconSize::Strip => px(15.0),
            IconSize::Control => px(17.0),
        }
    }
}

/// A Phosphor weight. Only [`VENDORED_WEIGHT`] is actually present in `assets/icons/` today - see
/// [`weight_for_size`] for why that is enough, and for what has to happen before it stops being.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconWeight {
    /// Phosphor `bold` - upstream `assets/bold/<name>-bold.svg`.
    Bold,
    /// Phosphor `regular` - upstream `assets/regular/<name>.svg`. Not vendored: nothing in this
    /// app draws an icon at 20px or above yet.
    Regular,
}

/// The one weight `assets/icons/` really holds. See that directory's `README.md` for the exact
/// upstream release and commit the files came from.
pub const VENDORED_WEIGHT: IconWeight = IconWeight::Bold;

/// The smallest box `regular` is allowed at, per `REVISION-2026-08-14.md` §8 ("`regular` only at
/// 20px+").
const REGULAR_WEIGHT_MIN_BOX: Pixels = px(20.0);

/// `REVISION-2026-08-14.md` §8's weight rule as real code: "`bold` at 15-17px (`regular`'s 1.5px
/// stroke reads thin against `#5e646a`); `regular` only at 20px+".
///
/// Pure and `Window`-free, so the rule is directly checkable - which is the point: a test pairs
/// this against [`VENDORED_WEIGHT`] for every [`IconSize`], so a future 20px+ size cannot land
/// without either vendoring the `regular` files or consciously restating the rule.
pub fn weight_for_size(box_size: Pixels) -> IconWeight {
    if box_size >= REGULAR_WEIGHT_MIN_BOX {
        IconWeight::Regular
    } else {
        IconWeight::Bold
    }
}

/// Which file an icon really resolves to right now: this app's own shipped asset, or a real
/// on-disk file from the user's active icon pack.
///
/// Split out of [`IconRow::draw`] as a pure function so the override rule ("shipped icons are the
/// default; a user icon pack still overrides by name") is testable against real temp directories
/// without a live GPUI window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconSource {
    /// This app's own vendored Phosphor file, by `gpui::AssetSource` path
    /// ([`Icon::asset_path`]).
    Shipped(&'static str),
    /// A real, existing file in the user's active icon pack, by absolute path.
    Pack(PathBuf),
}

/// [`IconSource`] for `icon` under `pack` - the pack's `<name>.svg` if it really exists
/// (`crate::icon_pack::resolve_icon` does a real `Path::is_file` check), otherwise the shipped
/// asset.
pub fn resolve_source(pack: &IconPackSettings, icon: Icon) -> IconSource {
    match icon_pack::resolve_icon(pack, icon.name()) {
        Some(path) => IconSource::Pack(path),
        None => IconSource::Shipped(icon.asset_path()),
    }
}

/// One row of icons sharing exactly one optical box - the only way to draw a shipped icon.
///
/// Constructed once per row with one [`IconSize`]; every [`Self::draw`] call uses that same box,
/// so `REVISION-2026-08-14.md` §7 rule 7 ("A row of icons needs one shared optical box, not one
/// size per icon") is structural here rather than a convention a call site can quietly break. A
/// single icon is a row of one - the rail footer's prune button is exactly that.
///
/// Borrows the settings' [`IconPackSettings`] rather than cloning it: a row is built inside a
/// render pass, which already has `&self`.
#[derive(Debug, Clone, Copy)]
pub struct IconRow<'a> {
    pack: &'a IconPackSettings,
    box_size: IconSize,
}

impl<'a> IconRow<'a> {
    /// A row whose every icon is drawn inside `box_size`'s square.
    pub fn new(pack: &'a IconPackSettings, box_size: IconSize) -> Self {
        Self { pack, box_size }
    }

    /// This row's shared optical box.
    pub fn size(&self) -> IconSize {
        self.box_size
    }

    /// `icon`, drawn inside this row's shared optical box and tinted `color`.
    ///
    /// The tint is applied with `.text_color(..)` because that is literally how GPUI colours an
    /// SVG: it rasterises the file to an alpha mask and paints a `MonochromeSprite` in the
    /// element's own text colour (see the module docs). It is applied to a pack override too -
    /// an SVG element with no text colour paints nothing at all in GPUI, so leaving the override
    /// untinted would ship a slot guaranteed to draw an empty box.
    ///
    /// Returns `gpui::Svg` rather than `AnyElement` so callers can still chain their own
    /// interactivity (`.id(..)`, `.on_click(..)`) onto the real element.
    pub fn draw(&self, icon: Icon, color: ColorToken) -> Svg {
        let source = resolve_source(self.pack, icon);
        let name = icon.name();
        let element = match &source {
            IconSource::Shipped(path) => gpui::svg().path(*path),
            IconSource::Pack(path) => gpui::svg().external_path(path.to_string_lossy().to_string()),
        };
        let box_size = self.box_size.box_size();
        let from_pack = matches!(source, IconSource::Pack(_));
        element
            .flex_none()
            .w(box_size)
            .h(box_size)
            .text_color(color)
            // Lets a real test measure this real icon's painted bounds (`debug_bounds` reads
            // this, not `.id(..)`) - a no-op outside test builds.
            .debug_selector(move || {
                if from_pack {
                    format!("icon-pack-{name}")
                } else {
                    format!("icon-{name}")
                }
            })
    }
}

/// The bytes of `path`, if it is one of the shipped icon assets - `crate::fonts::Assets`'
/// `gpui::AssetSource` implementation delegates here so every asset lookup for `icons/...`
/// has exactly one implementation.
pub(crate) fn load_asset(path: &str) -> Option<Cow<'static, [u8]>> {
    ICON_FILES
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, bytes)| Cow::Borrowed(*bytes))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::{
        div, px, AssetSource, Hsla, IntoElement, ParentElement, Render, Styled, SvgRenderer,
        TestAppContext,
    };

    use super::*;
    use crate::fonts::Assets;
    use crate::theme;

    fn no_pack() -> IconPackSettings {
        IconPackSettings { directory: None }
    }

    /// `REVISION-2026-08-14.md` §8's mapping table, transcribed: `(slot, icon, Phosphor name)`.
    /// Every other test in this module reads the glyph choice off this table rather than
    /// restating it, so the table is the single place the design is written down in code.
    const MAPPING: &[(&str, Icon, &str)] = &[
        ("strip: worktrees", Icon::TreeStructure, "tree-structure"),
        (
            "strip: history",
            Icon::ClockCounterClockwise,
            "clock-counter-clockwise",
        ),
        ("strip: problems", Icon::Warning, "warning"),
        ("tabs: Files", Icon::Folder, "folder"),
        ("tabs: Search", Icon::MagnifyingGlass, "magnifying-glass"),
        ("tabs: Changes", Icon::GitBranch, "git-branch"),
        (
            "count row: replace",
            Icon::ArrowsLeftRight,
            "arrows-left-right",
        ),
        ("count row: filter", Icon::Funnel, "funnel"),
        ("count row: fold-all", Icon::CaretDown, "caret-down"),
        ("rail footer: prune", Icon::Trash, "trash"),
        (
            "overflow menu: Settings",
            Icon::SlidersHorizontal,
            "sliders-horizontal",
        ),
        (
            "tab strip: terminal",
            Icon::TerminalWindow,
            "terminal-window",
        ),
    ];

    #[test]
    fn every_mapped_slot_from_the_design_handoff_has_its_phosphor_glyph() {
        assert_eq!(
            MAPPING.len(),
            Icon::ALL.len(),
            "REVISION-2026-08-14.md §8's table has {} rows - `Icon` must be exactly those slots, \
             no speculative extras (§7 rule 1: ship the affordance with the behaviour, or ship \
             neither)",
            MAPPING.len()
        );
        for (slot, icon, expected_name) in MAPPING {
            assert_eq!(
                icon.name(),
                *expected_name,
                "{slot} must map to Phosphor `{expected_name}`"
            );
            assert!(
                Icon::ALL.contains(icon),
                "{slot}'s icon is missing from `Icon::ALL`"
            );
        }
    }

    #[test]
    fn icon_files_and_the_icon_enum_are_the_same_set() {
        assert_eq!(
            ICON_FILES.len(),
            Icon::ALL.len(),
            "a file was vendored without an `Icon` variant, or the other way round"
        );
        for icon in Icon::ALL {
            assert!(
                ICON_FILES
                    .iter()
                    .any(|(path, _)| *path == icon.asset_path()),
                "{} has no entry in ICON_FILES",
                icon.asset_path()
            );
            assert_eq!(
                icon.asset_path(),
                format!("icons/{}.svg", icon.name()),
                "an icon's asset path must be derivable from its name - a user icon pack \
                 overrides by that same name"
            );
        }
    }

    #[test]
    fn icon_names_are_unique() {
        for (index, icon) in Icon::ALL.iter().enumerate() {
            for other in &Icon::ALL[index + 1..] {
                assert_ne!(
                    icon.name(),
                    other.name(),
                    "two icons share a name, so a user icon pack could never override just one"
                );
            }
        }
    }

    /// The shipped icons have to be reachable through the *app's own* `gpui::AssetSource`, since
    /// that is exactly what `gpui::svg().path(..)` loads from
    /// (`vendor/zed/crates/gpui/src/window.rs`'s `paint_svg` -> `SvgRenderer::render_alpha_mask`
    /// -> `asset_source.load(&params.path)`). A correct `asset_path()` that `Assets` does not
    /// serve would render nothing at all, silently.
    #[test]
    fn the_apps_asset_source_serves_every_shipped_icon() {
        let assets = Assets;
        for icon in Icon::ALL {
            let bytes = assets
                .load(icon.asset_path())
                .expect("load should not fail")
                .unwrap_or_else(|| panic!("{} is not served by Assets", icon.asset_path()));
            assert!(
                bytes.starts_with(b"<svg"),
                "{} does not begin with an <svg element",
                icon.asset_path()
            );
        }
    }

    #[test]
    fn the_apps_asset_source_lists_exactly_the_shipped_icons() {
        let assets = Assets;
        let listed = assets.list("icons").expect("list should not fail");
        assert_eq!(listed.len(), ICON_FILES.len());
        for icon in Icon::ALL {
            assert!(
                listed
                    .iter()
                    .any(|entry| entry.as_ref() == icon.asset_path()),
                "{} was not listed under `icons`",
                icon.asset_path()
            );
        }
    }

    /// Vendoring is only real if the files really are Phosphor's: one path on Phosphor's own
    /// `0 0 256 256` canvas, filled `currentColor`. The shared canvas is also what makes equal
    /// boxes give equal optical weight, so this is the sizing rule's own precondition.
    #[test]
    fn every_vendored_icon_is_a_real_phosphor_svg_on_the_shared_256_canvas() {
        for (path, bytes) in ICON_FILES {
            let text = std::str::from_utf8(bytes)
                .unwrap_or_else(|err| panic!("{path} is not valid UTF-8: {err}"));
            assert!(
                text.contains(r#"viewBox="0 0 256 256""#),
                "{path} is not on Phosphor's 256x256 canvas - two icons on different canvases \
                 cannot share one optical box"
            );
            assert!(
                text.contains(r#"fill="currentColor""#),
                "{path} is not a `currentColor` Phosphor file"
            );
            assert!(
                text.contains("<path d=\""),
                "{path} carries no real path data"
            );
        }
    }

    #[test]
    fn the_mit_licence_really_ships_beside_the_vendored_assets() {
        let licence = include_str!("../../../assets/icons/LICENSE.txt");
        assert!(
            licence.contains("MIT License"),
            "assets/icons/LICENSE.txt must be Phosphor's own MIT text"
        );
        assert!(
            licence.contains("Copyright (c) 2023 Phosphor Icons"),
            "assets/icons/LICENSE.txt must carry Phosphor's real copyright line"
        );
        assert!(
            licence.contains("THE SOFTWARE IS PROVIDED \"AS IS\""),
            "assets/icons/LICENSE.txt is truncated - the full MIT text has to ship"
        );

        let attribution = include_str!("../../../assets/icons/README.md");
        assert!(
            attribution.contains("https://github.com/phosphor-icons/core"),
            "the attribution note must name the real upstream repository"
        );
        assert!(
            attribution.contains("assets/bold/<name>-bold.svg"),
            "the attribution note must record which upstream weight these came from"
        );
    }

    // -- sizing -------------------------------------------------------------------------------

    /// The four named boxes are real measurements off the reviewed mock, not round numbers -
    /// pinned here so a later edit that "tidies" one of them has to argue with the design.
    #[test]
    fn named_sizes_match_the_mocks_own_boxes() {
        assert_eq!(IconSize::PanelTab.box_size(), px(11.0));
        assert_eq!(IconSize::TabChip.box_size(), px(14.0));
        assert_eq!(IconSize::Strip.box_size(), px(15.0));
        assert_eq!(IconSize::Control.box_size(), px(17.0));
    }

    /// §8's weight rule, checked against what `assets/icons/` really holds. This is the guard
    /// that makes adding a 20px+ size a deliberate act: it fails until `regular` is vendored.
    #[test]
    fn every_named_size_wants_the_weight_that_is_actually_vendored() {
        for size in IconSize::ALL {
            assert_eq!(
                weight_for_size(size.box_size()),
                VENDORED_WEIGHT,
                "{size:?} ({}) wants a Phosphor weight that is not vendored - REVISION-2026-08-14 \
                 §8: `bold` at 15-17px, `regular` only at 20px+",
                size.box_size()
            );
        }
    }

    #[test]
    fn the_weight_rule_flips_at_exactly_twenty_pixels() {
        assert_eq!(weight_for_size(px(15.0)), IconWeight::Bold);
        assert_eq!(weight_for_size(px(17.0)), IconWeight::Bold);
        assert_eq!(
            weight_for_size(px(19.9)),
            IconWeight::Bold,
            "below 20px is still bold - regular's 1.5px stroke reads thin there"
        );
        assert_eq!(
            weight_for_size(px(20.0)),
            IconWeight::Regular,
            "§8: `regular` only at 20px+"
        );
    }

    /// The whole of rule 7, as arithmetic on the real element: every icon in one row asks for
    /// the identical width and height, so no call site can hand one glyph a different size.
    #[test]
    fn a_row_gives_every_icon_the_identical_box() {
        let pack = no_pack();
        for size in IconSize::ALL {
            let row = IconRow::new(&pack, *size);
            let expected = gpui::Length::Definite(gpui::DefiniteLength::Absolute(
                gpui::AbsoluteLength::Pixels(size.box_size()),
            ));
            for icon in Icon::ALL {
                let mut element = row.draw(*icon, theme::text::FAINTER);
                let style = element.style();
                assert_eq!(
                    style.size.width,
                    Some(expected),
                    "{:?} at {size:?} did not take the row's shared box width",
                    icon.name()
                );
                assert_eq!(
                    style.size.height,
                    Some(expected),
                    "{:?} at {size:?} did not take the row's shared box height",
                    icon.name()
                );
            }
        }
    }

    /// Rule 7 again, but proved against real rasterised pixels rather than style values: GPUI's
    /// own `SvgRenderer` is handed each vendored file at one row's box, and every glyph in the
    /// row has to come back the same number of device pixels wide and tall. This is also the
    /// honest check that the vendored path data is real - a fabricated or truncated file either
    /// fails to parse here or rasterises blank.
    #[test]
    fn every_icon_really_rasterises_to_one_shared_box_through_gpuis_own_renderer() {
        let renderer = SvgRenderer::new(Arc::new(Assets));
        for size in IconSize::ALL {
            let mut row_dimensions = None;
            for (path, bytes) in ICON_FILES {
                // `render_single_frame` rasterises at `scale * SMOOTH_SVG_SCALE_FACTOR` against
                // the file's own 256px canvas, exactly as `paint_svg` does.
                let image = renderer
                    .render_single_frame(bytes, f32::from(size.box_size()) / 256.0)
                    .unwrap_or_else(|err| panic!("{path} failed to rasterise: {err:?}"));
                let dimensions = image.size(0);
                assert!(
                    dimensions.width.0 > 0 && dimensions.height.0 > 0,
                    "{path} rasterised to nothing at {size:?}"
                );
                match row_dimensions {
                    None => row_dimensions = Some(dimensions),
                    Some(expected) => assert_eq!(
                        dimensions, expected,
                        "{path} rasterised to a different box than the rest of the {size:?} row - \
                         rule 7: a row of icons needs one shared optical box"
                    ),
                }

                // GPUI keeps only the alpha channel of this raster (`paint_svg` ->
                // `render_alpha_mask`), so a file with no covered pixels would paint as nothing
                // at all while still "loading" fine.
                let raster = image
                    .as_bytes(0)
                    .unwrap_or_else(|| panic!("{path} has no frame data"));
                let covered = raster.chunks_exact(4).filter(|px| px[3] > 0).count();
                let total = raster.len() / 4;
                assert!(
                    covered * 20 >= total,
                    "{path} covers only {covered}/{total} pixels at {size:?} - at this size the \
                     glyph would read as nothing (REVISION-2026-08-14 §8's own complaint about \
                     the hand-drawn marks)"
                );
            }
        }
    }

    // -- the icon-pack override ---------------------------------------------------------------

    #[test]
    fn shipped_icons_are_the_default_source() {
        let pack = no_pack();
        for icon in Icon::ALL {
            assert_eq!(
                resolve_source(&pack, *icon),
                IconSource::Shipped(icon.asset_path())
            );
        }
    }

    #[test]
    fn a_user_icon_pack_overrides_a_shipped_icon_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let override_path = dir.path().join("folder.svg");
        std::fs::write(&override_path, "<svg></svg>").expect("write");
        let pack = IconPackSettings {
            directory: Some(dir.path().to_path_buf()),
        };

        assert_eq!(
            resolve_source(&pack, Icon::Folder),
            IconSource::Pack(override_path),
            "a real `<name>.svg` in the active pack must replace the shipped file for that slot"
        );
        assert_eq!(
            resolve_source(&pack, Icon::Trash),
            IconSource::Shipped(Icon::Trash.asset_path()),
            "a pack holding *some* icons must leave every other slot on the shipped default"
        );
    }

    #[test]
    fn a_pack_directory_with_no_matching_file_leaves_the_shipped_icon_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = IconPackSettings {
            directory: Some(dir.path().to_path_buf()),
        };
        for icon in Icon::ALL {
            assert_eq!(
                resolve_source(&pack, *icon),
                IconSource::Shipped(icon.asset_path())
            );
        }
    }

    // -- tinting ------------------------------------------------------------------------------

    /// GPUI paints an SVG as a `MonochromeSprite` in the element's own `text.color` and skips it
    /// entirely when that colour is `None`, so this assertion is the difference between a visible
    /// icon and an empty box.
    #[test]
    fn a_shipped_icon_is_tinted_with_the_callers_own_token() {
        let pack = no_pack();
        let row = IconRow::new(&pack, IconSize::Strip);
        for (token, label) in [
            (theme::text::FAINTER, "text.fainter"),
            (theme::status::FAIL, "status.fail"),
            (theme::text::BODY, "text.body"),
        ] {
            let mut element = row.draw(Icon::TreeStructure, token);
            assert_eq!(
                element.style().text.color,
                Some(Hsla::from(token)),
                "an icon must take {label}'s live resolved colour - GPUI tints the whole glyph \
                 with the element's text colour"
            );
        }
    }

    #[test]
    fn a_pack_override_is_tinted_too_rather_than_painting_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("trash.svg"), "<svg></svg>").expect("write");
        let pack = IconPackSettings {
            directory: Some(dir.path().to_path_buf()),
        };
        let mut element =
            IconRow::new(&pack, IconSize::Control).draw(Icon::Trash, theme::text::DIM);
        assert_eq!(
            element.style().text.color,
            Some(Hsla::from(theme::text::DIM)),
            "an svg element with no text colour is skipped outright by GPUI's paint path, so an \
             untinted override would ship a slot that draws an empty box"
        );
    }

    // -- a real window ------------------------------------------------------------------------

    /// A row of every shipped icon, rendered on its own so these assets can be exercised through
    /// a real GPUI window without depending on any of the not-yet-built surfaces that will
    /// eventually consume them.
    struct IconRowHarness {
        pack: IconPackSettings,
        size: IconSize,
    }

    impl Render for IconRowHarness {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            let row = IconRow::new(&self.pack, self.size);
            div().flex().children(
                Icon::ALL
                    .iter()
                    .map(|icon| row.draw(*icon, theme::text::FAINTER).into_any_element()),
            )
        }
    }

    /// Every icon, drawn standalone in a real window, really paints - and every one of them
    /// paints into the identical box. `debug_bounds` reads the *painted* frame, so this is the
    /// end-to-end version of the two assertions above: the asset genuinely loads through
    /// `crate::fonts::Assets`, and the row genuinely shares one optical box.
    #[gpui::test]
    fn every_icon_paints_into_the_rows_one_shared_box_in_a_real_window(cx: &mut TestAppContext) {
        // `debug_bounds` takes a `&'static str`, so the selectors cannot be interpolated from
        // `Icon::name()` - they are spelled out, and the length check below keeps the list total.
        const SELECTORS: &[&str] = &[
            "icon-arrows-left-right",
            "icon-caret-down",
            "icon-clock-counter-clockwise",
            "icon-folder",
            "icon-funnel",
            "icon-git-branch",
            "icon-magnifying-glass",
            "icon-sliders-horizontal",
            "icon-terminal-window",
            "icon-trash",
            "icon-tree-structure",
            "icon-warning",
        ];
        assert_eq!(
            SELECTORS.len(),
            Icon::ALL.len(),
            "this test's selector list is out of sync with `Icon::ALL`"
        );

        let (_view, cx) = cx.add_window_view(|_window, _cx| IconRowHarness {
            pack: no_pack(),
            size: IconSize::Control,
        });
        cx.run_until_parked();

        let expected = IconSize::Control.box_size();
        for selector in SELECTORS {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} did not paint at all"));
            assert_eq!(
                bounds.size.width, expected,
                "{selector} painted at a different width than the rest of its row"
            );
            assert_eq!(
                bounds.size.height, expected,
                "{selector} painted at a different height than the rest of its row"
            );
        }
    }

    /// The same window, with a real icon pack on disk: the overridden slot paints from the pack's
    /// own file (a different debug selector) while its neighbours keep painting the shipped ones.
    #[gpui::test]
    fn a_pack_override_paints_in_place_of_just_its_own_shipped_icon(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("folder.svg"),
            include_str!("../../../assets/icons/folder.svg"),
        )
        .expect("write");

        let (_view, cx) = cx.add_window_view(|_window, _cx| IconRowHarness {
            pack: IconPackSettings {
                directory: Some(dir.path().to_path_buf()),
            },
            size: IconSize::Strip,
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("icon-pack-folder").is_some(),
            "the pack's own `folder.svg` must paint in the Files slot"
        );
        assert!(
            cx.debug_bounds("icon-folder").is_none(),
            "the shipped folder icon must not also paint - the pack overrides it, not doubles it"
        );
        assert_eq!(
            cx.debug_bounds("icon-trash")
                .map(|bounds| bounds.size.width),
            Some(IconSize::Strip.box_size()),
            "every un-overridden slot must keep painting its shipped icon, in the same box"
        );
    }
}
