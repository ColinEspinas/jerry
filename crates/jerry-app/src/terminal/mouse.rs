//! xterm mouse reporting (GitHub issue #437): which pointer activity a program asked to hear
//! about, and the exact bytes one click, drag, hover, or wheel notch becomes on the wire.
//!
//! Pure - free of both `gpui` and `alacritty_terminal`, the same contract
//! [`crate::terminal::pane::keystroke_to_bytes`] holds for key input.
//! [`crate::terminal::grid`] reports the live protocol off `Term::mode()`, and
//! [`crate::terminal::pane`] converts its own GPUI event types into these plain ones. This module
//! owns no state and knows nothing about pixels, cell contents, or the pty.

/// Which pointer activity the running program asked to be told about (`DECSET 1000`/`1002`/`1003`).
///
/// One value rather than three booleans because the three are genuinely mutually exclusive:
/// `alacritty_terminal` clears the whole `TermMode::MOUSE_MODE` mask before setting any one of them
/// (`term/mod.rs:1954`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseTracking {
    /// The pointer is Jerry's own, for text selection and scrollback.
    Off,
    /// `?1000` - button presses and releases only.
    Clicks,
    /// `?1002` - presses and releases, plus motion while a button is held.
    ClicksAndDrag,
    /// `?1003` - presses and releases, plus every motion, button held or not.
    ClicksAndMotion,
}

/// How a report frames its coordinates (`DECSET 1006`/`1005`). Also mutually exclusive: each of the
/// two clears the other (`term/mod.rs:1972`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEncoding {
    /// The original framing: `CSI M <32+b> <32+1+x> <32+1+y>`, one byte per field, so no cell past
    /// index 222 can be named at all.
    Normal,
    /// `?1005`, deliberately only half-implemented: a report goes out only where the UTF-8 framing
    /// is byte-identical to [`Self::Normal`] (index <= 94, still a single ASCII byte), and anything
    /// beyond is dropped rather than encoded in a two-byte form this app does not implement.
    /// Nothing modern asks for `?1005` without also asking for `?1006`, which supersedes it.
    Utf8,
    /// `?1006` - `CSI < b ; x ; y M|m`. Decimal, unbounded, and the only framing that can say which
    /// button a release was for.
    Sgr,
}

/// The mouse-reporting protocol a program has active right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseProtocol {
    pub tracking: MouseTracking,
    pub encoding: MouseEncoding,
}

/// The three buttons xterm has a report code for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseReportButton {
    Left,
    Middle,
    Right,
}

/// What the pointer did, in the terminal's own vocabulary. An enum rather than a
/// `(kind, Option<button>)` pair so that the combinations with no meaning - a release of no button,
/// a wheel notch that can be let go - cannot be constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press(MouseReportButton),
    Release(MouseReportButton),
    /// The pointer entered a new cell; `Some` if a button was held while it did.
    Motion(Option<MouseReportButton>),
    WheelUp,
    WheelDown,
}

/// The three modifiers a report can carry. `gpui::Modifiers::platform` deliberately has no
/// counterpart - see [`MouseProtocol::encode`]'s caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// Where the pointer was, in the same 0-based viewport space
/// [`crate::terminal::grid::CellPosition`] uses. The protocol's own 1-based origin is applied
/// inside [`MouseProtocol::encode`], once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseCell {
    pub row: usize,
    pub column: usize,
}

/// Added to a button code to mark a report as "the pointer moved" rather than "a button changed
/// state", and the offset every field of the legacy framing carries.
const MOTION_BIT: u8 = 32;

/// The code meaning "no button": what a release reports in the framings that cannot name one, and -
/// once [`MOTION_BIT`] is added - what free hover reports under `?1003`.
const NO_BUTTON: u8 = 3;

impl MouseReportButton {
    fn code(self) -> u8 {
        match self {
            MouseReportButton::Left => 0,
            MouseReportButton::Middle => 1,
            MouseReportButton::Right => 2,
        }
    }

    /// A stable index for the caller to track press/release pairing with.
    pub fn index(self) -> usize {
        self.code() as usize
    }
}

impl MouseModifiers {
    fn bits(self) -> u8 {
        let mut bits = 0;
        if self.shift {
            bits += 4;
        }
        if self.alt {
            bits += 8;
        }
        if self.control {
            bits += 16;
        }
        bits
    }
}

impl MouseEncoding {
    /// The highest cell index this framing can name, or `None` if it has no ceiling.
    fn max_index(self) -> Option<usize> {
        match self {
            // `32 + 1 + 222 == 255`, the last value that still fits in the byte.
            MouseEncoding::Normal => Some(222),
            // `32 + 1 + 94 == 127`, the last index where the UTF-8 and single-byte framings agree.
            MouseEncoding::Utf8 => Some(94),
            MouseEncoding::Sgr => None,
        }
    }
}

impl MouseProtocol {
    /// Reporting turned off - what [`crate::terminal::pane`] substitutes whenever its own local
    /// gestures must win instead.
    pub const OFF: Self = Self {
        tracking: MouseTracking::Off,
        encoding: MouseEncoding::Normal,
    };

    /// Whether pointer events should reach the child at all.
    pub fn is_active(self) -> bool {
        self.tracking != MouseTracking::Off
    }

    /// The bytes a real terminal sends for `action` at `cell`, or `None` when this protocol has
    /// nothing to say about it: tracking off, a motion this tracking level never asked for, or a
    /// cell past what this encoding can name (see [`MouseEncoding`]).
    pub fn encode(
        self,
        action: MouseAction,
        cell: MouseCell,
        modifiers: MouseModifiers,
    ) -> Option<Vec<u8>> {
        if !self.reports(action) {
            return None;
        }
        let code = self.button_code(action) + modifiers.bits();
        match self.encoding {
            MouseEncoding::Sgr => Some(sgr_report(code, action, cell)),
            MouseEncoding::Normal | MouseEncoding::Utf8 => {
                let max_index = self.encoding.max_index()?;
                if cell.row > max_index || cell.column > max_index {
                    return None;
                }
                legacy_report(code, cell)
            }
        }
    }

    /// Whether this tracking level asked about `action`. Presses, releases, and wheel notches are
    /// reported at every level; motion is the levels' whole point of difference.
    fn reports(self, action: MouseAction) -> bool {
        match (self.tracking, action) {
            (MouseTracking::Off, _) => false,
            (MouseTracking::Clicks, MouseAction::Motion(_)) => false,
            (MouseTracking::ClicksAndDrag, MouseAction::Motion(held)) => held.is_some(),
            _ => true,
        }
    }

    /// The button code before modifier bits, which is where the legacy framings lose a release's
    /// button identity - SGR keeps it and marks the release with its final byte instead.
    fn button_code(self, action: MouseAction) -> u8 {
        match action {
            MouseAction::Press(button) => button.code(),
            MouseAction::Release(button) => match self.encoding {
                MouseEncoding::Sgr => button.code(),
                MouseEncoding::Normal | MouseEncoding::Utf8 => NO_BUTTON,
            },
            MouseAction::Motion(held) => {
                held.map_or(NO_BUTTON, MouseReportButton::code) + MOTION_BIT
            }
            MouseAction::WheelUp => 64,
            MouseAction::WheelDown => 65,
        }
    }
}

/// `?1006`: `CSI < code ; column ; row M|m`. Motion and wheel notches both end in `M` - only a
/// real release lowercases it.
fn sgr_report(code: u8, action: MouseAction, cell: MouseCell) -> Vec<u8> {
    let final_byte = if matches!(action, MouseAction::Release(_)) {
        'm'
    } else {
        'M'
    };
    format!(
        "\x1b[<{code};{};{}{final_byte}",
        cell.column + 1,
        cell.row + 1
    )
    .into_bytes()
}

/// The original framing: `CSI M` and one [`MOTION_BIT`]-offset byte per field. The caller has
/// already checked the cell against [`MouseEncoding::max_index`].
fn legacy_report(code: u8, cell: MouseCell) -> Option<Vec<u8>> {
    let offset = |index: usize| u8::try_from(index + 1 + MOTION_BIT as usize).ok();
    Some(vec![
        0x1b,
        b'[',
        b'M',
        MOTION_BIT.checked_add(code)?,
        offset(cell.column)?,
        offset(cell.row)?,
    ])
}

#[cfg(test)]
mod encoding_tests {
    use crate::terminal::mouse::{
        MouseAction, MouseCell, MouseEncoding, MouseModifiers, MouseProtocol, MouseReportButton,
        MouseTracking,
    };

    fn protocol(tracking: MouseTracking, encoding: MouseEncoding) -> MouseProtocol {
        MouseProtocol { tracking, encoding }
    }

    fn cell(row: usize, column: usize) -> MouseCell {
        MouseCell { row, column }
    }

    fn encode(protocol: MouseProtocol, action: MouseAction, cell: MouseCell) -> Option<Vec<u8>> {
        protocol.encode(action, cell, MouseModifiers::default())
    }

    #[test]
    fn sgr_press_reports_one_based_cell_coordinates() {
        let bytes = encode(
            protocol(MouseTracking::Clicks, MouseEncoding::Sgr),
            MouseAction::Press(MouseReportButton::Left),
            cell(1, 2),
        )
        .expect("a left press under click tracking must be reportable");
        assert_eq!(
            bytes,
            b"\x1b[<0;3;2M".to_vec(),
            "viewport row 1 / column 2 is reported 1-based as column 3, row 2"
        );
    }

    #[test]
    fn sgr_release_keeps_the_button_and_lowercases_the_final_byte() {
        let bytes = encode(
            protocol(MouseTracking::Clicks, MouseEncoding::Sgr),
            MouseAction::Release(MouseReportButton::Right),
            cell(0, 0),
        )
        .expect("a right release under click tracking must be reportable");
        assert_eq!(bytes, b"\x1b[<2;1;1m".to_vec());
    }

    #[test]
    fn the_middle_and_right_buttons_report_codes_one_and_two() {
        for (button, code) in [
            (MouseReportButton::Left, b'0'),
            (MouseReportButton::Middle, b'1'),
            (MouseReportButton::Right, b'2'),
        ] {
            let bytes = encode(
                protocol(MouseTracking::Clicks, MouseEncoding::Sgr),
                MouseAction::Press(button),
                cell(0, 0),
            )
            .expect("every button must be reportable");
            assert_eq!(
                bytes,
                vec![0x1b, b'[', b'<', code, b';', b'1', b';', b'1', b'M'],
                "{button:?} must report its own code"
            );
        }
    }

    #[test]
    fn the_normal_encoding_offsets_every_field_by_thirty_two() {
        let bytes = encode(
            protocol(MouseTracking::Clicks, MouseEncoding::Normal),
            MouseAction::Press(MouseReportButton::Left),
            cell(1, 2),
        )
        .expect("a left press under click tracking must be reportable");
        assert_eq!(
            bytes,
            vec![0x1b, b'[', b'M', 32, 32 + 3, 32 + 2],
            "button 0, then column 2 and row 1, each made 1-based and offset by 32"
        );
    }

    #[test]
    fn the_normal_encoding_cannot_say_which_button_was_released() {
        let left = encode(
            protocol(MouseTracking::Clicks, MouseEncoding::Normal),
            MouseAction::Release(MouseReportButton::Left),
            cell(0, 0),
        )
        .expect("a release must be reportable");
        let right = encode(
            protocol(MouseTracking::Clicks, MouseEncoding::Normal),
            MouseAction::Release(MouseReportButton::Right),
            cell(0, 0),
        )
        .expect("a release must be reportable");
        assert_eq!(
            left, right,
            "the legacy framing has no room for the button identity"
        );
        assert_eq!(left, vec![0x1b, b'[', b'M', 32 + 3, 33, 33]);
    }

    #[test]
    fn motion_sets_the_thirty_two_bit_and_free_hover_reports_thirty_five() {
        let dragging = encode(
            protocol(MouseTracking::ClicksAndDrag, MouseEncoding::Sgr),
            MouseAction::Motion(Some(MouseReportButton::Middle)),
            cell(0, 0),
        )
        .expect("motion with a button held is reportable under drag tracking");
        assert_eq!(dragging, b"\x1b[<33;1;1M".to_vec(), "1 + 32");

        let hovering = encode(
            protocol(MouseTracking::ClicksAndMotion, MouseEncoding::Sgr),
            MouseAction::Motion(None),
            cell(0, 0),
        )
        .expect("free motion is reportable under motion tracking");
        assert_eq!(hovering, b"\x1b[<35;1;1M".to_vec(), "3 + 32");
    }

    #[test]
    fn the_wheel_is_button_sixty_four_up_and_sixty_five_down() {
        let up = encode(
            protocol(MouseTracking::Clicks, MouseEncoding::Sgr),
            MouseAction::WheelUp,
            cell(0, 0),
        )
        .expect("a wheel notch is reportable at every tracking level");
        assert_eq!(up, b"\x1b[<64;1;1M".to_vec());

        let down = encode(
            protocol(MouseTracking::Clicks, MouseEncoding::Sgr),
            MouseAction::WheelDown,
            cell(0, 0),
        )
        .expect("a wheel notch is reportable at every tracking level");
        assert_eq!(down, b"\x1b[<65;1;1M".to_vec());
    }

    #[test]
    fn modifiers_add_shift_four_alt_eight_and_control_sixteen() {
        let bytes = protocol(MouseTracking::Clicks, MouseEncoding::Sgr)
            .encode(
                MouseAction::Press(MouseReportButton::Left),
                cell(0, 0),
                MouseModifiers {
                    shift: false,
                    alt: true,
                    control: true,
                },
            )
            .expect("a modified press must still be reportable");
        assert_eq!(bytes, b"\x1b[<24;1;1M".to_vec(), "0 + 8 + 16 = 24");
    }

    #[test]
    fn the_normal_encoding_drops_a_cell_past_its_own_ceiling() {
        let press = MouseAction::Press(MouseReportButton::Left);
        let normal = protocol(MouseTracking::Clicks, MouseEncoding::Normal);
        assert!(
            encode(normal, press, cell(0, 222)).is_some(),
            "index 222 is the last one the framing can name"
        );
        assert!(
            encode(normal, press, cell(0, 223)).is_none(),
            "a cell with no legacy representation is dropped, never wrapped into a different one"
        );
        assert!(
            encode(normal, press, cell(223, 0)).is_none(),
            "the ceiling applies to the row axis too"
        );
        assert!(
            encode(
                protocol(MouseTracking::Clicks, MouseEncoding::Sgr),
                press,
                cell(500, 500)
            )
            .is_some(),
            "SGR has no ceiling"
        );
    }

    #[test]
    fn the_utf8_encoding_is_emitted_only_where_it_matches_the_normal_one_byte_for_byte() {
        let press = MouseAction::Press(MouseReportButton::Left);
        let utf8 = protocol(MouseTracking::Clicks, MouseEncoding::Utf8);
        let normal = protocol(MouseTracking::Clicks, MouseEncoding::Normal);
        assert_eq!(
            encode(utf8, press, cell(0, 94)),
            encode(normal, press, cell(0, 94)),
            "up to index 94 the two framings agree exactly"
        );
        assert!(
            encode(utf8, press, cell(0, 95)).is_none(),
            "past that this app drops the report rather than invent the two-byte form"
        );
    }

    #[test]
    fn tracking_off_encodes_nothing() {
        for action in [
            MouseAction::Press(MouseReportButton::Left),
            MouseAction::Release(MouseReportButton::Left),
            MouseAction::Motion(Some(MouseReportButton::Left)),
            MouseAction::Motion(None),
            MouseAction::WheelUp,
            MouseAction::WheelDown,
        ] {
            assert!(
                encode(MouseProtocol::OFF, action, cell(0, 0)).is_none(),
                "a program that never asked for the mouse must not receive {action:?}"
            );
        }
    }

    #[test]
    fn click_tracking_reports_no_motion_at_all() {
        let clicks = protocol(MouseTracking::Clicks, MouseEncoding::Sgr);
        assert!(encode(clicks, MouseAction::Motion(None), cell(0, 0)).is_none());
        assert!(encode(
            clicks,
            MouseAction::Motion(Some(MouseReportButton::Left)),
            cell(0, 0)
        )
        .is_none());
    }

    #[test]
    fn drag_tracking_reports_motion_only_while_a_button_is_held() {
        let drag = protocol(MouseTracking::ClicksAndDrag, MouseEncoding::Sgr);
        assert!(encode(drag, MouseAction::Motion(None), cell(0, 0)).is_none());
        assert!(encode(
            drag,
            MouseAction::Motion(Some(MouseReportButton::Left)),
            cell(0, 0)
        )
        .is_some());
    }

    #[test]
    fn motion_tracking_reports_button_less_motion() {
        assert!(encode(
            protocol(MouseTracking::ClicksAndMotion, MouseEncoding::Sgr),
            MouseAction::Motion(None),
            cell(0, 0)
        )
        .is_some());
    }
}
