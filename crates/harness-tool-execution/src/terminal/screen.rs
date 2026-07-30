#[derive(Default)]
struct TerminalScreen {
    lines: Vec<Vec<char>>,
    row: usize,
    column: usize,
    saved_cursor: Option<(usize, usize)>,
    state: ParserState,
}

#[derive(Default)]
enum ParserState {
    #[default]
    Ground,
    Escape,
    Csi(String),
    Osc { escape: bool },
    StringControl { escape: bool },
}

impl TerminalScreen {
    fn render(bytes: &[u8]) -> String {
        let mut screen = Self::default();
        for character in String::from_utf8_lossy(bytes).chars() {
            screen.consume(character);
        }
        screen.contents()
    }

    fn consume(&mut self, character: char) {
        let state = std::mem::take(&mut self.state);
        match state {
            ParserState::Ground => self.consume_ground(character),
            ParserState::Escape => self.consume_escape(character),
            ParserState::Csi(mut parameters) => {
                if ('@'..='~').contains(&character) {
                    self.apply_csi(&parameters, character);
                    self.state = ParserState::Ground;
                } else {
                    parameters.push(character);
                    self.state = ParserState::Csi(parameters);
                }
            }
            ParserState::Osc { escape } => {
                if character == '\u{7}' || escape && character == '\\' {
                    self.state = ParserState::Ground;
                } else {
                    self.state = ParserState::Osc {
                        escape: character == '\u{1b}',
                    };
                }
            }
            ParserState::StringControl { escape } => {
                if escape && character == '\\' {
                    self.state = ParserState::Ground;
                } else {
                    self.state = ParserState::StringControl {
                        escape: character == '\u{1b}',
                    };
                }
            }
        }
    }

    fn consume_ground(&mut self, character: char) {
        match character {
            '\u{1b}' => self.state = ParserState::Escape,
            '\r' => self.column = 0,
            '\n' => self.row = self.row.saturating_add(1),
            '\u{8}' => self.column = self.column.saturating_sub(1),
            '\t' => {
                self.column = self.column.saturating_add(8 - self.column % 8);
            }
            character if character.is_control() => {}
            character => self.write(character),
        }
    }

    fn consume_escape(&mut self, character: char) {
        self.state = ParserState::Ground;
        match character {
            '[' => self.state = ParserState::Csi(String::new()),
            ']' => self.state = ParserState::Osc { escape: false },
            'P' | 'X' | '^' | '_' => {
                self.state = ParserState::StringControl { escape: false };
            }
            '7' => self.saved_cursor = Some((self.row, self.column)),
            '8' => {
                if let Some((row, column)) = self.saved_cursor {
                    self.row = row;
                    self.column = column;
                }
            }
            'D' => self.row = self.row.saturating_add(1),
            'E' => {
                self.row = self.row.saturating_add(1);
                self.column = 0;
            }
            'M' => self.row = self.row.saturating_sub(1),
            'c' => self.clear(),
            _ => {}
        }
    }

    fn apply_csi(&mut self, parameters: &str, command: char) {
        let parameters = parameters
            .trim_start_matches(['?', '>', '<', '='])
            .split(';')
            .map(|value| value.parse::<usize>().ok())
            .collect::<Vec<_>>();
        let parameter = |index: usize, default: usize| {
            parameters
                .get(index)
                .and_then(|value| *value)
                .filter(|value| *value != 0)
                .unwrap_or(default)
        };
        match command {
            'A' => self.row = self.row.saturating_sub(parameter(0, 1)),
            'B' => self.row = self.row.saturating_add(parameter(0, 1)),
            'C' => self.column = self.column.saturating_add(parameter(0, 1)),
            'D' => self.column = self.column.saturating_sub(parameter(0, 1)),
            'E' => {
                self.row = self.row.saturating_add(parameter(0, 1));
                self.column = 0;
            }
            'F' => {
                self.row = self.row.saturating_sub(parameter(0, 1));
                self.column = 0;
            }
            'G' | '`' => self.column = parameter(0, 1).saturating_sub(1),
            'H' | 'f' => {
                self.row = parameter(0, 1).saturating_sub(1);
                self.column = parameter(1, 1).saturating_sub(1);
            }
            'd' => self.row = parameter(0, 1).saturating_sub(1),
            'J' => self.erase_display(parameters.first().and_then(|value| *value).unwrap_or(0)),
            'K' => self.erase_line(parameters.first().and_then(|value| *value).unwrap_or(0)),
            'P' => self.delete_characters(parameter(0, 1)),
            '@' => self.insert_blanks(parameter(0, 1)),
            'X' => self.erase_characters(parameter(0, 1)),
            's' => self.saved_cursor = Some((self.row, self.column)),
            'u' => {
                if let Some((row, column)) = self.saved_cursor {
                    self.row = row;
                    self.column = column;
                }
            }
            _ => {}
        }
    }

    fn line_mut(&mut self) -> &mut Vec<char> {
        if self.lines.len() <= self.row {
            self.lines.resize_with(self.row + 1, Vec::new);
        }
        &mut self.lines[self.row]
    }

    fn write(&mut self, character: char) {
        let column = self.column;
        let line = self.line_mut();
        if line.len() < column {
            line.resize(column, ' ');
        }
        if column == line.len() {
            line.push(character);
        } else {
            line[column] = character;
        }
        self.column = self.column.saturating_add(1);
    }

    fn erase_line(&mut self, mode: usize) {
        let column = self.column;
        let line = self.line_mut();
        match mode {
            1 => {
                let end = column.saturating_add(1).min(line.len());
                line[..end].fill(' ');
            }
            2 => line.clear(),
            _ => line.truncate(column.min(line.len())),
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            1 => {
                for line in self.lines.iter_mut().take(self.row) {
                    line.clear();
                }
                self.erase_line(1);
            }
            2 | 3 => self.clear(),
            _ => {
                self.erase_line(0);
                self.lines.truncate(self.row.saturating_add(1));
            }
        }
    }

    fn delete_characters(&mut self, count: usize) {
        let column = self.column;
        let line = self.line_mut();
        if column < line.len() {
            let end = column.saturating_add(count).min(line.len());
            line.drain(column..end);
        }
    }

    fn insert_blanks(&mut self, count: usize) {
        let column = self.column;
        let line = self.line_mut();
        if line.len() < column {
            line.resize(column, ' ');
        }
        line.splice(column..column, std::iter::repeat_n(' ', count));
    }

    fn erase_characters(&mut self, count: usize) {
        let column = self.column;
        let line = self.line_mut();
        if line.len() < column.saturating_add(count) {
            line.resize(column.saturating_add(count), ' ');
        }
        line[column..column.saturating_add(count)].fill(' ');
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.row = 0;
        self.column = 0;
        self.saved_cursor = None;
    }

    fn contents(&self) -> String {
        let last_content_row = self
            .lines
            .iter()
            .rposition(|line| line.iter().any(|character| *character != ' '));
        let Some(last_row) = last_content_row.map_or_else(
            || (self.row > 0).then_some(self.row),
            |last_content_row| Some(last_content_row.max(self.row)),
        ) else {
            return String::new();
        };

        let mut output = String::new();
        for row in 0..=last_row {
            if let Some(line) = self.lines.get(row) {
                let end = line
                    .iter()
                    .rposition(|character| *character != ' ')
                    .map_or(0, |index| index + 1);
                output.extend(line[..end].iter());
            }
            if row < last_row {
                output.push('\n');
            }
        }
        output
    }
}

pub(super) fn render(bytes: &[u8]) -> String {
    TerminalScreen::render(bytes)
}

pub(super) fn delta(previous: &str, current: &str, reset: bool) -> String {
    if reset || previous.is_empty() {
        return current.to_owned();
    }
    if previous == current {
        return String::new();
    }
    if let Some(appended) = current.strip_prefix(previous) {
        return appended.to_owned();
    }

    let mut common = previous
        .bytes()
        .zip(current.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while common > 0 && !current.is_char_boundary(common) {
        common -= 1;
    }
    let changed_line = current[..common]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    current[changed_line..].to_owned()
}
