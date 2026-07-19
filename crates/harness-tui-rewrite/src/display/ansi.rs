//! Terminal-control parser used by the raw display transition.

use crate::control::{
    APPLICATION_PROGRAM_COMMAND, BACKSPACE, BELL, CARRIAGE_RETURN, CONTROL_SEQUENCE_INTRODUCER,
    DEVICE_CONTROL_STRING, ESCAPE, HORIZONTAL_TAB, LINE_FEED, OPERATING_SYSTEM_COMMAND,
    PRIVACY_MESSAGE, START_OF_STRING, STRING_TERMINATOR,
};

/// Seven-bit CSI introducer byte following Escape.
const CSI_INTRODUCER: char = '[';
/// Seven-bit OSC introducer byte following Escape.
const OSC_INTRODUCER: char = ']';
/// Seven-bit DCS introducer byte following Escape.
const DCS_INTRODUCER: char = 'P';
/// Seven-bit SOS introducer byte following Escape.
const SOS_INTRODUCER: char = 'X';
/// Seven-bit PM introducer byte following Escape.
const PM_INTRODUCER: char = '^';
/// Seven-bit APC introducer byte following Escape.
const APC_INTRODUCER: char = '_';
/// Final byte following Escape that completes a seven-bit String Terminator.
const STRING_TERMINATOR_FINAL: char = '\\';
/// SGR final byte within a Control Sequence.
const SGR_FINAL: char = 'm';
/// ECMA-48 range for a CSI final byte.
const CSI_FINAL_START: char = '\u{0040}';
const CSI_FINAL_END: char = '\u{007e}';
/// ECMA-48 range for CSI parameter bytes.
const CSI_PARAMETER_START: char = '\u{0030}';
const CSI_PARAMETER_END: char = '\u{003f}';
/// ECMA-48 range for CSI intermediate bytes.
const CSI_INTERMEDIATE_START: char = '\u{0020}';
const CSI_INTERMEDIATE_END: char = '\u{002f}';
/// Prevents an unterminated CSI sequence from retaining unbounded parameters.
const MAX_CSI_PARAMETER_BYTES: usize = 128;
/// Prompt-independent terminal tab stop used for captured tool output.
const TERMINAL_TAB_WIDTH: usize = 4;

use super::{AnsiColor, AnsiStyle, DocumentLine, DocumentRun, RawFragment, StyleId, TextSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    Csi,
    Osc,
    DeviceControl,
    StringControl,
    EscapeInStringControl(StringFamily),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringFamily {
    Osc,
    DeviceControl,
    Other,
}

struct FragmentParser {
    source: TextSource,
    base_style: StyleId,
    selectable: bool,
    state: ParserState,
    ansi_style: AnsiStyle,
    csi_parameters: String,
    lines: Vec<DocumentLine>,
    current_runs: Vec<DocumentRun>,
    current_text: String,
}

pub(super) fn parse_fragments(fragments: Vec<RawFragment>) -> Vec<DocumentLine> {
    let mut lines = vec![DocumentLine { runs: Vec::new() }];
    for fragment in fragments {
        let parsed = FragmentParser::new(&fragment).parse(&fragment.text);
        append_fragment_lines(&mut lines, parsed);
    }
    if lines.is_empty() {
        lines.push(DocumentLine { runs: Vec::new() });
    }
    lines
}

fn append_fragment_lines(target: &mut Vec<DocumentLine>, mut source: Vec<DocumentLine>) {
    if source.is_empty() {
        return;
    }
    let first = source.remove(0);
    target
        .last_mut()
        .expect("display document always has one line")
        .runs
        .extend(first.runs);
    target.extend(source);
}

impl FragmentParser {
    fn new(fragment: &RawFragment) -> Self {
        Self {
            source: fragment.source,
            base_style: fragment.style,
            selectable: fragment.selectable,
            state: ParserState::Ground,
            ansi_style: AnsiStyle::default(),
            csi_parameters: String::new(),
            lines: vec![DocumentLine { runs: Vec::new() }],
            current_runs: Vec::new(),
            current_text: String::new(),
        }
    }

    fn parse(mut self, input: &str) -> Vec<DocumentLine> {
        for character in input.chars() {
            self.consume(character);
        }
        self.flush_text();
        self.flush_runs();
        self.lines
    }

    fn consume(&mut self, character: char) {
        match self.state {
            ParserState::Ground => self.consume_ground(character),
            ParserState::Escape => self.consume_escape(character),
            ParserState::Csi => self.consume_csi(character),
            ParserState::Osc => self.consume_string(character, StringFamily::Osc),
            ParserState::DeviceControl => {
                self.consume_string(character, StringFamily::DeviceControl);
            }
            ParserState::StringControl => self.consume_string(character, StringFamily::Other),
            ParserState::EscapeInStringControl(family) => {
                if character == STRING_TERMINATOR_FINAL {
                    self.state = ParserState::Ground;
                } else {
                    self.state = match family {
                        StringFamily::Osc => ParserState::Osc,
                        StringFamily::DeviceControl => ParserState::DeviceControl,
                        StringFamily::Other => ParserState::StringControl,
                    };
                }
            }
        }
    }

    fn consume_ground(&mut self, character: char) {
        match character {
            ESCAPE => {
                self.flush_text();
                self.state = ParserState::Escape;
            }
            CONTROL_SEQUENCE_INTRODUCER => {
                self.flush_text();
                self.csi_parameters.clear();
                self.state = ParserState::Csi;
            }
            OPERATING_SYSTEM_COMMAND => {
                self.flush_text();
                self.state = ParserState::Osc;
            }
            DEVICE_CONTROL_STRING => {
                self.flush_text();
                self.state = ParserState::DeviceControl;
            }
            START_OF_STRING | PRIVACY_MESSAGE | APPLICATION_PROGRAM_COMMAND => {
                self.flush_text();
                self.state = ParserState::StringControl;
            }
            LINE_FEED => self.newline(),
            HORIZONTAL_TAB => self.expand_tab(),
            CARRIAGE_RETURN | BACKSPACE => {}
            character if character.is_control() => {}
            character => self.current_text.push(character),
        }
    }

    fn consume_escape(&mut self, character: char) {
        self.state = match character {
            CSI_INTRODUCER => {
                self.csi_parameters.clear();
                ParserState::Csi
            }
            OSC_INTRODUCER => ParserState::Osc,
            DCS_INTRODUCER => ParserState::DeviceControl,
            SOS_INTRODUCER | PM_INTRODUCER | APC_INTRODUCER => ParserState::StringControl,
            _ => ParserState::Ground,
        };
    }

    fn consume_csi(&mut self, character: char) {
        if (CSI_FINAL_START..=CSI_FINAL_END).contains(&character) {
            if character == SGR_FINAL && self.source == TextSource::Terminal {
                apply_sgr(&self.csi_parameters, &mut self.ansi_style);
            }
            self.csi_parameters.clear();
            self.state = ParserState::Ground;
        } else if (CSI_PARAMETER_START..=CSI_PARAMETER_END).contains(&character)
            || (CSI_INTERMEDIATE_START..=CSI_INTERMEDIATE_END).contains(&character)
        {
            if self.csi_parameters.len() < MAX_CSI_PARAMETER_BYTES {
                self.csi_parameters.push(character);
            }
        } else {
            self.csi_parameters.clear();
            self.state = ParserState::Ground;
        }
    }

    fn consume_string(&mut self, character: char, family: StringFamily) {
        match character {
            BELL if family == StringFamily::Osc => self.state = ParserState::Ground,
            STRING_TERMINATOR => self.state = ParserState::Ground,
            ESCAPE => self.state = ParserState::EscapeInStringControl(family),
            _ => {}
        }
    }

    fn expand_tab(&mut self) {
        let current = self.current_text.chars().count();
        let spaces = TERMINAL_TAB_WIDTH - (current % TERMINAL_TAB_WIDTH);
        self.current_text.extend(std::iter::repeat_n(' ', spaces));
    }

    fn newline(&mut self) {
        self.flush_text();
        self.flush_runs();
        self.lines.push(DocumentLine { runs: Vec::new() });
    }

    fn flush_text(&mut self) {
        if self.current_text.is_empty() {
            return;
        }
        let style =
            if self.source == TextSource::Terminal && self.ansi_style != AnsiStyle::default() {
                StyleId::Ansi(self.ansi_style)
            } else {
                self.base_style
            };
        self.current_runs.push(DocumentRun {
            text: std::mem::take(&mut self.current_text),
            style,
            selectable: self.selectable,
        });
    }

    fn flush_runs(&mut self) {
        self.lines
            .last_mut()
            .expect("fragment parser always has one line")
            .runs
            .append(&mut self.current_runs);
    }
}

const SGR_RESET: u16 = 0;
const SGR_BOLD: u16 = 1;
const SGR_DIM: u16 = 2;
const SGR_REVERSE: u16 = 7;
const SGR_NORMAL_INTENSITY: u16 = 22;
const SGR_REVERSE_OFF: u16 = 27;
const SGR_FOREGROUND_START: u16 = 30;
const SGR_FOREGROUND_END: u16 = 37;
const SGR_EXTENDED_FOREGROUND: u16 = 38;
const SGR_DEFAULT_FOREGROUND: u16 = 39;
const SGR_BACKGROUND_START: u16 = 40;
const SGR_BACKGROUND_END: u16 = 47;
const SGR_EXTENDED_BACKGROUND: u16 = 48;
const SGR_DEFAULT_BACKGROUND: u16 = 49;
const SGR_BRIGHT_FOREGROUND_START: u16 = 90;
const SGR_BRIGHT_FOREGROUND_END: u16 = 97;
const SGR_BRIGHT_BACKGROUND_START: u16 = 100;
const SGR_BRIGHT_BACKGROUND_END: u16 = 107;
const SGR_INDEXED_COLOR_MODE: u16 = 5;
const SGR_RGB_COLOR_MODE: u16 = 2;

fn apply_sgr(parameters: &str, style: &mut AnsiStyle) {
    let codes = if parameters.is_empty() {
        vec![SGR_RESET]
    } else {
        parameters
            .split(';')
            .map(|part| part.parse::<u16>().unwrap_or(u16::MAX))
            .collect::<Vec<_>>()
    };

    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            SGR_RESET => *style = AnsiStyle::default(),
            SGR_BOLD => style.bold = true,
            SGR_DIM => style.dim = true,
            SGR_NORMAL_INTENSITY => {
                style.bold = false;
                style.dim = false;
            }
            SGR_REVERSE => style.reversed = true,
            SGR_REVERSE_OFF => style.reversed = false,
            SGR_FOREGROUND_START..=SGR_FOREGROUND_END => {
                style.foreground = Some(AnsiColor::Basic {
                    index: (codes[index] - SGR_FOREGROUND_START) as u8,
                    bright: false,
                });
            }
            SGR_DEFAULT_FOREGROUND => style.foreground = None,
            SGR_BACKGROUND_START..=SGR_BACKGROUND_END => {
                style.background = Some(AnsiColor::Basic {
                    index: (codes[index] - SGR_BACKGROUND_START) as u8,
                    bright: false,
                });
            }
            SGR_DEFAULT_BACKGROUND => style.background = None,
            SGR_BRIGHT_FOREGROUND_START..=SGR_BRIGHT_FOREGROUND_END => {
                style.foreground = Some(AnsiColor::Basic {
                    index: (codes[index] - SGR_BRIGHT_FOREGROUND_START) as u8,
                    bright: true,
                });
            }
            SGR_BRIGHT_BACKGROUND_START..=SGR_BRIGHT_BACKGROUND_END => {
                style.background = Some(AnsiColor::Basic {
                    index: (codes[index] - SGR_BRIGHT_BACKGROUND_START) as u8,
                    bright: true,
                });
            }
            SGR_EXTENDED_FOREGROUND | SGR_EXTENDED_BACKGROUND => {
                let foreground = codes[index] == SGR_EXTENDED_FOREGROUND;
                if let Some((color, consumed)) = extended_color(&codes[index + 1..]) {
                    if foreground {
                        style.foreground = Some(color);
                    } else {
                        style.background = Some(color);
                    }
                    index += consumed;
                }
            }
            _ => {}
        }
        index += 1;
    }
}

fn extended_color(codes: &[u16]) -> Option<(AnsiColor, usize)> {
    match codes {
        [SGR_INDEXED_COLOR_MODE, index, ..] => {
            Some((AnsiColor::Indexed((*index).min(255) as u8), 2))
        }
        [SGR_RGB_COLOR_MODE, red, green, blue, ..] => Some((
            AnsiColor::Rgb(
                (*red).min(255) as u8,
                (*green).min(255) as u8,
                (*blue).min(255) as u8,
            ),
            4,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_terminal(text: &str) -> Vec<DocumentLine> {
        parse_fragments(vec![RawFragment {
            text: text.to_string(),
            style: StyleId::Plain,
            selectable: true,
            source: TextSource::Terminal,
        }])
    }

    fn visible_text(lines: &[DocumentLine]) -> String {
        lines
            .iter()
            .map(|line| {
                line.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parser_discards_incomplete_escape_sequences() {
        for input in ["before\u{1b}", "before\u{1b}[31", "before\u{1b}]title"] {
            assert_eq!(visible_text(&parse_terminal(input)), "before");
        }
    }

    #[test]
    fn parser_discards_string_control_payloads() {
        assert_eq!(
            visible_text(&parse_terminal(
                "a\u{1b}]title\u{7}b\u{1b}Ppayload\u{1b}\\c"
            )),
            "abc"
        );
    }

    #[test]
    fn parser_preserves_sgr_as_typed_style() {
        let lines = parse_terminal("plain \u{1b}[31mred\u{1b}[0m done");
        assert_eq!(visible_text(&lines), "plain red done");
        assert!(matches!(
            lines[0].runs[1].style,
            StyleId::Ansi(AnsiStyle {
                foreground: Some(AnsiColor::Basic {
                    index: 1,
                    bright: false
                }),
                ..
            })
        ));
    }
}
