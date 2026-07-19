//! Named control characters and directional-formatting classification.
//!
//! These constants describe protocol code points. They do not authorize direct
//! terminal output; the terminal backend remains the only control-sequence
//! writer.

/// ECMA-48 Escape starts a seven-bit terminal control sequence.
pub(crate) const ESCAPE: char = '\u{001b}';
/// ASCII Bell may terminate an Operating System Command sequence.
pub(crate) const BELL: char = '\u{0007}';
/// ASCII Backspace is cursor movement in terminal output.
pub(crate) const BACKSPACE: char = '\u{0008}';
/// ASCII Horizontal Tab advances to a terminal tab stop.
pub(crate) const HORIZONTAL_TAB: char = '\u{0009}';
/// ASCII Line Feed is represented as a structural display line break.
pub(crate) const LINE_FEED: char = '\u{000a}';
/// ASCII Carriage Return moves to the start of a terminal row.
pub(crate) const CARRIAGE_RETURN: char = '\u{000d}';

/// C1 Device Control String starts a terminal device-control payload.
pub(crate) const DEVICE_CONTROL_STRING: char = '\u{0090}';
/// C1 Start of String starts a generic terminal string-control payload.
pub(crate) const START_OF_STRING: char = '\u{0098}';
/// C1 Control Sequence Introducer starts a CSI command.
pub(crate) const CONTROL_SEQUENCE_INTRODUCER: char = '\u{009b}';
/// C1 String Terminator ends OSC, DCS, SOS, PM, or APC payloads.
pub(crate) const STRING_TERMINATOR: char = '\u{009c}';
/// C1 Operating System Command starts an OSC payload.
pub(crate) const OPERATING_SYSTEM_COMMAND: char = '\u{009d}';
/// C1 Privacy Message starts a generic string-control payload.
pub(crate) const PRIVACY_MESSAGE: char = '\u{009e}';
/// C1 Application Program Command starts a generic string-control payload.
pub(crate) const APPLICATION_PROGRAM_COMMAND: char = '\u{009f}';

/// First C0 control code point, used for control-picture projection.
pub(crate) const C0_CONTROL_START: char = '\u{0000}';
/// Last C0 control code point, used for control-picture projection.
pub(crate) const C0_CONTROL_END: char = '\u{001f}';
/// Unicode Delete control character.
pub(crate) const DELETE: char = '\u{007f}';
/// Offset from a C0 code point to its Unicode Control Pictures glyph.
pub(crate) const C0_CONTROL_PICTURE_OFFSET: u32 = 0x2400;
/// Unicode Symbol for Delete used to display an exact pasted Delete character.
pub(crate) const DELETE_CONTROL_PICTURE: char = '\u{2421}';

/// Returns whether a character changes Unicode bidirectional presentation.
///
/// These format controls are removed from runtime output and visibly
/// substituted in exact user-owned prompt text.
pub(crate) fn is_directional_formatting(character: char) -> bool {
    const ARABIC_LETTER_MARK: char = '\u{061c}';
    const LEFT_TO_RIGHT_MARK: char = '\u{200e}';
    const RIGHT_TO_LEFT_MARK: char = '\u{200f}';
    const EMBEDDING_START: char = '\u{202a}';
    const OVERRIDE_END: char = '\u{202e}';
    const ISOLATE_START: char = '\u{2066}';
    const ISOLATE_END: char = '\u{2069}';

    matches!(
        character,
        ARABIC_LETTER_MARK | LEFT_TO_RIGHT_MARK | RIGHT_TO_LEFT_MARK
    ) || (EMBEDDING_START..=OVERRIDE_END).contains(&character)
        || (ISOLATE_START..=ISOLATE_END).contains(&character)
}
