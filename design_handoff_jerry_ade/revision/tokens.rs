// Jerry — colour tokens transcribed from the reviewed mockup (Jerry.dc.html).
// Every value is a flat fill or a 1px border colour. No gradients, no blur.
// Adapt the type to your GPUI theme layer (Hsla::from(rgb(0x…)) etc.).

pub mod surface {
    pub const WINDOW: u32 = 0x0e0f11; // window body
    pub const WINDOW_BORDER: u32 = 0x262a2e;
    pub const TITLE_BAR: u32 = 0x101214;
    pub const RAIL: u32 = 0x101113; // left rail + right panel
    pub const CENTER: u32 = 0x131518; // work surface
    pub const PTY: u32 = 0x0d0f11; // agent CLI + terminal
    pub const HEADER: u32 = 0x121417; // context bar, panel headers
    pub const FOOTER: u32 = 0x111316; // surface footers, status strips
    pub const CARD: u32 = 0x161a1d; // composer, settings cards
    pub const CARD_SUNK: u32 = 0x131619; // card footers
    pub const POPOVER: u32 = 0x181c20; // completion popup, hover card
    pub const PALETTE: u32 = 0x15181b;
    pub const SCRIM: u32 = 0x060708; // at 62% alpha behind the palette
    pub const ROW_HOVER: u32 = 0x15181b;
    pub const ROW_HOVER_ALT: u32 = 0x1b1f22; // hover on chrome buttons
    pub const ROW_SELECTED: u32 = 0x1a1e21;
    pub const SEGMENT_TRACK: u32 = 0x171a1d;
    pub const SEGMENT_ACTIVE: u32 = 0x242a2f;
    pub const KEYCAP: u32 = 0x181c1f;
    pub const KEYCAP_HINT: u32 = 0x15181a; // 14-high hint keycap
    pub const CAPTION_CLOSE_HOVER: u32 = 0x8c3a38; // Windows close button
    pub const ENV_CHIP: u32 = 0x16222c; // WSL chip bg
    pub const CHIP_NEUTRAL: u32 = 0x23272b;
    pub const CURRENT_LINE: u32 = 0x181c20;
}

pub mod border {
    pub const ZONE: u32 = 0x1e2225; // between the three zones
    pub const INNER: u32 = 0x1c2023; // between bands inside a zone
    pub const RAIL_INNER: u32 = 0x191c1f;
    pub const ROW: u32 = 0x171a1c; // change-list row separators
    pub const DIVIDER: u32 = 0x22262a; // 1px vertical rules
    pub const CARD: u32 = 0x23282c;
    pub const CARD_FIELD: u32 = 0x22272b;
    pub const COMPOSER: u32 = 0x24292e;
    pub const POPOVER: u32 = 0x2b3238;
    pub const BUTTON: u32 = 0x2a2f34; // outline button
    pub const BUTTON_DISABLED: u32 = 0x1f2327;
    pub const KEYCAP: u32 = 0x272c31;
    pub const KEYCAP_HINT: u32 = 0x23272b;
    pub const ENV_CHIP: u32 = 0x24384a;
    pub const SELECTED_EDGE: u32 = 0x3f5b74; // 2px left edge on a selected row
}

pub mod text {
    pub const SELECTED: u32 = 0xdde2e7;
    pub const PRIMARY: u32 = 0xd3d8dd;
    pub const HEADING: u32 = 0xc8cdd2;
    pub const STRONG: u32 = 0xc2c7cc;
    pub const BODY: u32 = 0xb8bfc6;
    pub const SECONDARY: u32 = 0xa9b0b7;
    pub const MUTED: u32 = 0x9aa1a8;
    pub const DIM: u32 = 0x8b9197;
    pub const DIMMER: u32 = 0x7d848b;
    pub const FAINT: u32 = 0x6b7178;
    pub const FAINTER: u32 = 0x5e646a;
    pub const GHOST: u32 = 0x4e545a;
    pub const GHOSTER: u32 = 0x454b51;
    pub const HINT: u32 = 0x41464b;
    pub const GUTTER: u32 = 0x3a3f44;
    pub const DISABLED: u32 = 0x3d4248;
    pub const LINK: u32 = 0x7fb4e3; // terminal paths, file:line
    pub const LINK_HOVER: u32 = 0xa5cdf0;
    pub const LINK_UNDERLINE: u32 = 0x3d6a91; // 1px dotted
    pub const LINK_UNDERLINE_HOVER: u32 = 0x78a8d0;
    pub const ENV_CHIP: u32 = 0x8fbde6;
    pub const CONFIG_SECTION: u32 = 0xc294e0; // toml/json section line
}

/// Status is the only place colour carries meaning in the rail.
pub mod status {
    pub const ASK: u32 = 0xe2a336; // needs input
    pub const ASK_BG: u32 = 0x3a2c14;
    pub const FAIL: u32 = 0xe0625c;
    pub const FAIL_BG: u32 = 0x3a1e1e;
    pub const REVIEW: u32 = 0x5cb87f;
    pub const REVIEW_BG: u32 = 0x1e3b2a;
    pub const RUN: u32 = 0x5a9ad4;
    pub const RUN_BG: u32 = 0x1e2f3e;
    pub const IDLE: u32 = 0x565d64;
    pub const IDLE_BG: u32 = 0x22262a;
    // waiting-question preview inside a rail row
    pub const ASK_CARD_BG: u32 = 0x1c1710;
    pub const ASK_CARD_EDGE: u32 = 0x8a6420;
    pub const ASK_CARD_FG: u32 = 0xc99b4e;
    // conflict banner
    pub const BANNER_BG: u32 = 0x1b1610;
    pub const BANNER_BORDER: u32 = 0x33291a;
}

pub mod diff {
    pub const ADD_BG: u32 = 0x12211a;
    pub const ADD_FG: u32 = 0x9fd0b2;
    pub const ADD_SIGN: u32 = 0x4e8c68;
    pub const DEL_BG: u32 = 0x211517;
    pub const DEL_FG: u32 = 0xd6a4a0;
    pub const DEL_SIGN: u32 = 0xa35f5b;
    pub const CTX_FG: u32 = 0x868d94;
    pub const HUNK_BG: u32 = 0x15181c;
    pub const HUNK_FG: u32 = 0x5f666e;
    pub const FOLD_BG: u32 = 0x121417;
    pub const FOLD_FG: u32 = 0x4a5057;
    pub const STAT_ADD: u32 = 0x5f9c78; // "+142" label
    pub const STAT_DEL: u32 = 0xb06a66; // "−8" label
    pub const STAT_EMPTY: u32 = 0x22262a; // unused segment of the 5-bar
    pub const GIT_GUTTER: u32 = 0x2c6244; // 3px agent-touched marker
}

pub mod syntax {
    pub const TEXT: u32 = 0xacb2be;
    pub const KEYWORD: u32 = 0xb477cf;
    pub const FUNCTION: u32 = 0x74ade8;
    pub const TYPE: u32 = 0xdfc184;
    pub const LITERAL: u32 = 0xbf956a; // numbers, `self`
    pub const COMMENT: u32 = 0x5d636f;
    pub const CARET: u32 = 0x5a9ad4;
    pub const ERROR_UNDERLINE: u32 = 0xe0625c; // 2px dotted
    pub const HOVER_UNDERLINE: u32 = 0x4d7ba8; // 1px solid
}

pub mod term {
    pub const PROMPT: u32 = 0x8fbde6;
    pub const TEXT: u32 = 0xa7adb4;
    pub const DIM: u32 = 0x6b7178;
    pub const OK: u32 = 0x6ab97f;
    pub const ERR: u32 = 0xe0625c;
    pub const WARN: u32 = 0xd8a94a;
    pub const HEADING: u32 = 0xced4da;
    pub const ACTIVITY: u32 = 0x5a9ad4; // spinner / progress line
    pub const MENU_SEL_FG: u32 = 0xe0b263;
    pub const MENU_SEL_BG: u32 = 0x1f1a10;
    pub const CURSOR: u32 = 0x5a9ad4;
}

/// One tint per agent. Used on the rail badge, the CLI tab chip and the
/// conflict side headers, so a colour always means the same agent.
pub mod agent {
    pub const SONNET: (u32, u32) = (0xd8a94a, 0x33280f); // (fg, bg)
    pub const CODEX: (u32, u32) = (0x6ab97f, 0x1e3327);
    pub const HAIKU: (u32, u32) = (0xc98fbf, 0x332030);
    pub const LOCAL: (u32, u32) = (0x7f9ad4, 0x1f2941);
}

/// Language chips, shared by the file tree, the code tab and the palette.
pub mod lang {
    pub const RS: (u32, u32) = (0xc0824a, 0x2e2113); // "rs"
    pub const TOML: (u32, u32) = (0x8b9197, 0x23272b); // "to"
    pub const MD: (u32, u32) = (0x7f9ad4, 0x1d2532); // "md"
    pub const SQL: (u32, u32) = (0x6ab97f, 0x1b2a20); // "sq"
    pub const TS: (u32, u32) = (0x6b9bd1, 0x1b2838); // "ts"
    pub const VUE: (u32, u32) = (0x5cb87f, 0x16261e); // "vue"
    pub const PY: (u32, u32) = (0xc9b04a, 0x2a2612); // "py"
    pub const GO: (u32, u32) = (0x5fa8c4, 0x152730); // "go"
    pub const UNKNOWN: (u32, u32) = (0x6b7178, 0x23272b); // "·"
}

pub mod button {
    pub const GREEN_BG: u32 = 0x24503a;
    pub const GREEN_BG_HOVER: u32 = 0x2c6045;
    pub const GREEN_FG: u32 = 0x9fdcb6;
    pub const GREEN_KEYCAP: u32 = 0x376b4d;
    pub const BLUE_BG: u32 = 0x243c50;
    pub const BLUE_BG_HOVER: u32 = 0x2c4a63;
    pub const BLUE_FG: u32 = 0xa5cdf0;
    pub const BLUE_KEYCAP: u32 = 0x365b78;
    pub const AMBER_BG: u32 = 0x3a2c14;
    pub const AMBER_BG_HOVER: u32 = 0x4a3818;
    pub const AMBER_FG: u32 = 0xe0b263;
    pub const DANGER_FG: u32 = 0xc4726d;
    pub const DANGER_FG_HOVER: u32 = 0xe3908b;
}

pub mod toggle {
    pub const TRACK_ON: u32 = 0x2f6d4b;
    pub const TRACK_OFF: u32 = 0x23272b;
    pub const KNOB_ON: u32 = 0xc8ecd6;
    pub const KNOB_OFF: u32 = 0x6b7178;
}

pub mod tag {
    pub const NEW: (u32, u32) = (0x7fc79a, 0x1e3b2a);
    pub const DELETED: (u32, u32) = (0xd18b86, 0x3a1e1e);
    pub const CONFLICT: (u32, u32) = (0xe0b263, 0x3a2c14);
    pub const TREE_ADDED: u32 = 0x5f9c78; // "A" mark
    pub const TREE_MODIFIED: u32 = 0xa3873f; // "M" mark
}

pub mod radius {
    pub const WINDOW: f32 = 10.0;
    pub const PANEL: f32 = 8.0; // palette
    pub const CARD: f32 = 6.0;
    pub const CARD_SM: f32 = 5.0;
    pub const BUTTON: f32 = 4.0;
    pub const CHIP: f32 = 3.0; // chips, keycaps, segments
    pub const MARK: f32 = 2.0; // stat bars, small squares
    pub const PILL: f32 = 8.0; // toggle track (26x15)
}

pub mod band {
    pub const TITLE_BAR: f32 = 38.0;
    pub const TAB_STRIP: f32 = 34.0;
    pub const RAIL_HEADER: f32 = 36.0;
    pub const PANEL_HEADER: f32 = 36.0;
    pub const CONTEXT_BAR: f32 = 32.0;
    pub const DIFF_TOOLBAR: f32 = 31.0;
    pub const FILTER_ROW: f32 = 30.0;
    pub const SURFACE_FOOTER: f32 = 28.0;
    pub const PTY_HEADER: f32 = 27.0;
    pub const BREADCRUMB: f32 = 26.0;
    pub const STATUS_BAR: f32 = 28.0;
    pub const TERM_FOOTER: f32 = 26.0;
    pub const PALETTE_INPUT: f32 = 44.0;
    pub const PALETTE_ROW: f32 = 30.0;
    pub const CHANGE_ROW: f32 = 27.0;
    pub const TREE_ROW: f32 = 22.0;
    pub const KEYCAP: f32 = 15.0;
    pub const KEYCAP_HINT: f32 = 14.0;
    pub const CAPTION_BUTTON_W: f32 = 44.0;
}

pub mod zone {
    pub const RAIL_WIDTH: f32 = 276.0; // adjustable 240..=340
    pub const PANEL_WIDTH: f32 = 320.0;
    pub const PANEL_WIDTH_EMPTY: f32 = 260.0;
    pub const SETTINGS_NAV_WIDTH: f32 = 212.0;
    pub const SETTINGS_CONTENT_MAX: f32 = 700.0;
    pub const TAB_MENU_WIDTH: f32 = 326.0;
    pub const THEME_CARD_WIDTH: f32 = 212.0;
    pub const PALETTE_WIDTH: f32 = 684.0;
    pub const COMPOSER_WIDTH: f32 = 560.0;
}

/// The only shadows in the product. Drop them if GPUI makes them awkward —
/// the borders carry the elevation on their own.
pub mod shadow {
    pub const POPOVER: (f32, f32, f32) = (0.0, 8.0, 20.0); // rgba(0,0,0,0.50)
    pub const PALETTE: (f32, f32, f32) = (0.0, 12.0, 34.0); // rgba(0,0,0,0.55)
}
