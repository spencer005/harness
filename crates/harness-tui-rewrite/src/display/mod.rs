//! Construction and layout of terminal-safe display documents.
//!
//! A document advances through consuming typestate transitions. Only the
//! `LaidOut` stage can expose Ratatui lines.

mod ansi;
mod layout;

use std::{marker::PhantomData, ops::Range};

pub(crate) use layout::LaidOutLine;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::control::is_directional_formatting;

/// Semantic style selected by presentation code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) enum StyleId {
    /// Default terminal text.
    #[default]
    Plain,
    /// Muted metadata and continuation text.
    Muted,
    /// Assistant message marker and text.
    Assistant,
    /// User message marker and text.
    User,
    /// Developer message marker and text.
    Developer,
    /// Tool call or output text.
    Tool,
    /// Error text.
    Error,
    /// Active status text.
    Active,
    /// Queued steering text.
    Queued,
    /// Selected input text.
    Selection,
    /// Status bar background.
    Status,
    /// Prompt background.
    Prompt,
    /// ANSI-defined foreground and background colors.
    Ansi(AnsiStyle),
}

/// Terminal colors retained from supported SGR sequences.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct AnsiStyle {
    foreground: Option<AnsiColor>,
    background: Option<AnsiColor>,
    bold: bool,
    dim: bool,
    reversed: bool,
}

/// Terminal color represented independently from Ratatui.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AnsiColor {
    Basic { index: u8, bright: bool },
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Untrusted fragments have not passed terminal-control parsing.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Raw;

/// Parsed fragments contain visible text and structural lines, but Unicode
/// control validation has not completed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Parsed;

/// Control-free fragments contain only permitted visible Unicode text.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ControlFree;

/// Bounded fragments satisfy configured byte, line, and cell limits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounded;

/// Laid-out fragments have terminal-cell wrapping and source mappings.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LaidOut;

/// Parsing policy for one untrusted fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextSource {
    /// Plain application or runtime text strips terminal formatting.
    Plain,
    /// Terminal output interprets supported SGR sequences.
    Terminal,
}

#[derive(Debug, Clone)]
struct RawFragment {
    text: String,
    style: StyleId,
    selectable: bool,
    source: TextSource,
}

#[derive(Debug, Clone)]
struct DocumentLine {
    runs: Vec<DocumentRun>,
}

#[derive(Debug, Clone)]
struct DocumentRun {
    text: String,
    style: StyleId,
    selectable: bool,
}

#[derive(Debug, Clone)]
enum DocumentStorage {
    Raw(Vec<RawFragment>),
    Lines(Vec<DocumentLine>),
    Layout(LayoutStorage),
}

#[derive(Debug, Clone)]
struct LayoutStorage {
    lines: Vec<LaidOutLine>,
}

/// Display document parameterized by its validation stage.
#[derive(Debug, Clone)]
pub(crate) struct DisplayDocument<State> {
    storage: DocumentStorage,
    _state: PhantomData<State>,
}

/// Builder for an untrusted display document.
#[derive(Debug, Default)]
pub(crate) struct RawDocumentBuilder {
    fragments: Vec<RawFragment>,
}

impl RawDocumentBuilder {
    /// Creates an empty raw document builder.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Appends an untrusted plain-text fragment.
    pub(crate) fn plain(
        &mut self,
        text: impl Into<String>,
        style: StyleId,
        selectable: bool,
    ) -> &mut Self {
        self.fragments.push(RawFragment {
            text: text.into(),
            style,
            selectable,
            source: TextSource::Plain,
        });
        self
    }

    /// Appends untrusted terminal output whose SGR styles are interpreted.
    pub(crate) fn terminal(
        &mut self,
        text: impl Into<String>,
        style: StyleId,
        selectable: bool,
    ) -> &mut Self {
        self.fragments.push(RawFragment {
            text: text.into(),
            style,
            selectable,
            source: TextSource::Terminal,
        });
        self
    }

    /// Appends a structural line break.
    pub(crate) fn line_break(&mut self) -> &mut Self {
        self.fragments.push(RawFragment {
            text: "\n".to_string(),
            style: StyleId::Plain,
            selectable: false,
            source: TextSource::Plain,
        });
        self
    }

    /// Finishes construction of the raw document.
    pub(crate) fn build(self) -> DisplayDocument<Raw> {
        DisplayDocument {
            storage: DocumentStorage::Raw(self.fragments),
            _state: PhantomData,
        }
    }
}

/// Limits applied before width-dependent layout.
///
/// Byte and cell limits account for visible run text. Structural line breaks
/// are accounted for by `max_lines`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DocumentLimits {
    max_bytes: usize,
    max_lines: usize,
    max_cells: usize,
}

impl DocumentLimits {
    /// Limits a normal transcript entry.
    pub(crate) const TRANSCRIPT_ENTRY: Self = Self::new(64 * 1024, 256, 128 * 1024);

    /// Limits compact status and activity documents.
    pub(crate) const UI_LABEL: Self = Self::new(8 * 1024, 8, 16 * 1024);

    /// Limits terminal tool output before viewport wrapping.
    pub(crate) const TOOL_OUTPUT: Self = Self::new(64 * 1024, 96, 128 * 1024);

    const fn new(max_bytes: usize, max_lines: usize, max_cells: usize) -> Self {
        assert!(max_bytes >= layout::OMISSION_MARKER_BYTES);
        assert!(max_lines > 0);
        assert!(max_cells >= layout::OMISSION_MARKER_CELLS);
        Self {
            max_bytes,
            max_lines,
            max_cells,
        }
    }
    /// Covers one already viewport-bounded prompt projection without truncation.
    pub(crate) fn prompt_viewport(bytes: usize, lines: usize, cells: usize) -> Self {
        Self::new(
            bytes.max(layout::OMISSION_MARKER_BYTES),
            lines.max(1),
            cells.max(layout::OMISSION_MARKER_CELLS),
        )
    }
}

impl DisplayDocument<Raw> {
    /// Parses escape sequences and structural controls.
    pub(crate) fn parse(self) -> DisplayDocument<Parsed> {
        let DocumentStorage::Raw(fragments) = self.storage else {
            unreachable!("raw document stores raw fragments");
        };
        DisplayDocument {
            storage: DocumentStorage::Lines(ansi::parse_fragments(fragments)),
            _state: PhantomData,
        }
    }
}

impl DisplayDocument<Parsed> {
    /// Removes forbidden Unicode controls and proves the control-free stage.
    pub(crate) fn sanitize(self) -> DisplayDocument<ControlFree> {
        let DocumentStorage::Lines(mut lines) = self.storage else {
            unreachable!("parsed document stores lines");
        };
        for line in &mut lines {
            for run in &mut line.runs {
                run.text.retain(is_permitted_display_character);
            }
            line.runs.retain(|run| !run.text.is_empty());
        }
        if lines.is_empty() {
            lines.push(DocumentLine { runs: Vec::new() });
        }
        debug_assert!(lines.iter().all(|line| {
            line.runs
                .iter()
                .all(|run| run.text.chars().all(is_permitted_display_character))
        }));
        DisplayDocument {
            storage: DocumentStorage::Lines(lines),
            _state: PhantomData,
        }
    }
}

impl DisplayDocument<ControlFree> {
    /// Applies deterministic resource limits and proves the bounded stage.
    pub(crate) fn bound(self, limits: DocumentLimits) -> DisplayDocument<Bounded> {
        let DocumentStorage::Lines(lines) = self.storage else {
            unreachable!("control-free document stores lines");
        };
        let lines = layout::bound_lines(lines, limits);
        DisplayDocument {
            storage: DocumentStorage::Lines(lines),
            _state: PhantomData,
        }
    }

    /// Restricts a compact label to one explicit terminal row.
    pub(crate) fn bound_one_line(
        self,
        limits: DocumentLimits,
        width: u16,
    ) -> DisplayDocument<Bounded> {
        let DocumentStorage::Lines(lines) = self.storage else {
            unreachable!("control-free document stores lines");
        };
        let lines = layout::bound_one_line(lines, limits, usize::from(width));
        DisplayDocument {
            storage: DocumentStorage::Lines(lines),
            _state: PhantomData,
        }
    }

    /// Returns control-free selectable text for clipboard construction.
    pub(crate) fn selectable_text(&self) -> String {
        let DocumentStorage::Lines(lines) = &self.storage else {
            unreachable!("control-free document stores lines");
        };
        selectable_text(lines)
    }
}

impl DisplayDocument<Bounded> {
    /// Wraps the document to `width` terminal cells.
    pub(crate) fn layout(self, width: u16) -> DisplayDocument<LaidOut> {
        let DocumentStorage::Lines(lines) = self.storage else {
            unreachable!("bounded document stores lines");
        };
        let layout = layout::layout_lines(lines, usize::from(width.max(1)));
        DisplayDocument {
            storage: DocumentStorage::Layout(layout),
            _state: PhantomData,
        }
    }
}

impl DisplayDocument<LaidOut> {
    /// Returns terminal-safe laid-out lines.
    pub(crate) fn lines(&self) -> &[LaidOutLine] {
        let DocumentStorage::Layout(layout) = &self.storage else {
            unreachable!("laid-out document stores layout");
        };
        &layout.lines
    }

    /// Converts validated lines to Ratatui lines at the terminal backend.
    pub(crate) fn ratatui_lines(&self) -> Vec<Line<'static>> {
        self.lines()
            .iter()
            .map(|line| {
                Line::from(
                    line.runs()
                        .iter()
                        .map(|run| Span::styled(run.text().to_string(), ratatui_style(run.style())))
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }
    /// Converts one validated line and optional semantic selection to Ratatui.
    pub(crate) fn ratatui_line(
        line: &LaidOutLine,
        selection: Option<&Range<usize>>,
    ) -> Line<'static> {
        let mut spans = Vec::new();
        for run in line.runs() {
            let Some(run_selection) = run.selection_bytes() else {
                spans.push(Span::styled(
                    run.text().to_string(),
                    ratatui_style(run.style()),
                ));
                continue;
            };
            let Some(selection) = selection else {
                spans.push(Span::styled(
                    run.text().to_string(),
                    ratatui_style(run.style()),
                ));
                continue;
            };
            let intersection_start = run_selection.start.max(selection.start);
            let intersection_end = run_selection.end.min(selection.end);
            if intersection_start >= intersection_end {
                spans.push(Span::styled(
                    run.text().to_string(),
                    ratatui_style(run.style()),
                ));
                continue;
            }

            let local_start = intersection_start.saturating_sub(run_selection.start);
            let local_end = intersection_end.saturating_sub(run_selection.start);
            debug_assert!(run.text().is_char_boundary(local_start));
            debug_assert!(run.text().is_char_boundary(local_end));
            let style = ratatui_style(run.style());
            if local_start > 0 {
                spans.push(Span::styled(run.text()[..local_start].to_string(), style));
            }
            spans.push(Span::styled(
                run.text()[local_start..local_end].to_string(),
                style.add_modifier(Modifier::REVERSED),
            ));
            if local_end < run.text().len() {
                spans.push(Span::styled(run.text()[local_end..].to_string(), style));
            }
        }
        Line::from(spans)
    }
}

/// Runs the complete validation pipeline for a raw document.
pub(crate) fn prepare_document(
    raw: DisplayDocument<Raw>,
    limits: DocumentLimits,
    width: u16,
) -> DisplayDocument<LaidOut> {
    raw.parse().sanitize().bound(limits).layout(width)
}

/// Runs validation and explicit one-row truncation for compact UI labels.
pub(crate) fn prepare_one_line_document(
    raw: DisplayDocument<Raw>,
    limits: DocumentLimits,
    width: u16,
) -> DisplayDocument<LaidOut> {
    raw.parse()
        .sanitize()
        .bound_one_line(limits, width)
        .layout(width)
}

fn selectable_text(lines: &[DocumentLine]) -> String {
    let mut output = String::new();
    for (line_index, line) in lines.iter().enumerate() {
        if line_index > 0 {
            output.push('\n');
        }
        for run in &line.runs {
            if run.selectable {
                output.push_str(&run.text);
            }
        }
    }
    output
}

fn is_permitted_display_character(character: char) -> bool {
    !character.is_control() && !is_directional_formatting(character)
}

/// Returns the Ratatui style for a semantic style at the backend boundary.
pub(crate) fn backend_style(style: StyleId) -> Style {
    ratatui_style(style)
}

fn ratatui_style(style: StyleId) -> Style {
    match style {
        StyleId::Plain => Style::default().fg(Color::Rgb(220, 220, 224)),
        StyleId::Muted => Style::default().fg(Color::DarkGray),
        StyleId::Assistant => Style::default().fg(Color::Rgb(198, 190, 232)),
        StyleId::User => Style::default().fg(Color::Rgb(150, 200, 218)),
        StyleId::Developer => Style::default().fg(Color::Rgb(218, 184, 116)),
        StyleId::Tool => Style::default().fg(Color::Rgb(188, 154, 208)),
        StyleId::Error => Style::default().fg(Color::LightRed),
        StyleId::Active => Style::default().fg(Color::LightCyan),
        StyleId::Queued => Style::default().fg(Color::LightYellow),
        StyleId::Selection => Style::default().add_modifier(Modifier::REVERSED),
        StyleId::Status => Style::default()
            .fg(Color::Rgb(208, 208, 214))
            .bg(Color::Rgb(18, 18, 24)),
        StyleId::Prompt => Style::default()
            .fg(Color::Rgb(226, 220, 206))
            .bg(Color::Rgb(24, 24, 32)),
        StyleId::Ansi(ansi) => ansi_ratatui_style(ansi),
    }
}

fn ansi_ratatui_style(ansi: AnsiStyle) -> Style {
    let mut style = Style::default();
    if let Some(foreground) = ansi.foreground {
        style = style.fg(ansi_color(foreground));
    }
    if let Some(background) = ansi.background {
        style = style.bg(ansi_color(background));
    }
    if ansi.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if ansi.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    if ansi.reversed {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn ansi_color(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Basic { index, bright } => match (index, bright) {
            (0, false) => Color::Black,
            (1, false) => Color::Red,
            (2, false) => Color::Green,
            (3, false) => Color::Yellow,
            (4, false) => Color::Blue,
            (5, false) => Color::Magenta,
            (6, false) => Color::Cyan,
            (7, false) => Color::Gray,
            (0, true) => Color::DarkGray,
            (1, true) => Color::LightRed,
            (2, true) => Color::LightGreen,
            (3, true) => Color::LightYellow,
            (4, true) => Color::LightBlue,
            (5, true) => Color::LightMagenta,
            (6, true) => Color::LightCyan,
            (7, true) => Color::White,
            _ => Color::Reset,
        },
        AnsiColor::Indexed(index) => Color::Indexed(index),
        AnsiColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

/// Control-free clipboard payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClipboardText(String);

impl ClipboardText {
    const MAX_BYTES: usize = 64 * 1024;

    /// Constructs a clipboard payload from text already proven control-free.
    pub(crate) fn from_control_free(text: String) -> Option<Self> {
        if text.is_empty()
            || text.len() > Self::MAX_BYTES
            || !text.chars().all(is_permitted_display_character_or_newline)
        {
            return None;
        }
        Some(Self(text))
    }

    /// Returns the validated clipboard text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_permitted_display_character_or_newline(character: char) -> bool {
    character == '\n' || is_permitted_display_character(character)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str, source: TextSource) -> DisplayDocument<ControlFree> {
        let mut builder = RawDocumentBuilder::new();
        match source {
            TextSource::Plain => {
                builder.plain(text, StyleId::Plain, true);
            }
            TextSource::Terminal => {
                builder.terminal(text, StyleId::Plain, true);
            }
        }
        builder.build().parse().sanitize()
    }

    #[test]
    fn control_free_stage_removes_escape_families_and_bidi_controls() {
        let control_free = document(
            "before\u{1b}[31mred\u{1b}[0m\u{1b}]0;title\u{7}after\u{202e}x",
            TextSource::Terminal,
        );
        assert_eq!(control_free.selectable_text(), "beforeredafterx");
        assert!(
            control_free
                .selectable_text()
                .chars()
                .all(is_permitted_display_character_or_newline)
        );
    }

    #[test]
    fn plain_source_never_interprets_sgr_as_visible_text() {
        let control_free = document("a\u{1b}[31mb", TextSource::Plain);
        assert_eq!(control_free.selectable_text(), "ab");
    }

    #[test]
    fn clipboard_rejects_controls_and_oversized_payloads() {
        assert!(ClipboardText::from_control_free("valid\ntext".to_string()).is_some());
        assert!(ClipboardText::from_control_free("bad\u{1b}".to_string()).is_none());
        assert!(
            ClipboardText::from_control_free("x".repeat(ClipboardText::MAX_BYTES + 1)).is_none()
        );
    }
}
