//! Canonical grapheme and terminal-cell layout for prompt interaction.

use std::{cell::Cell, ops::Range};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{PromptPosition, projection};

const ROW_CHECKPOINT_INTERVAL: usize = 128;
const MAX_VIEWPORT_PROJECTION_BYTES: usize = 256 * 1024;
const MAX_VIEWPORT_INTERIOR_STOPS: usize = 64 * 1024;
const OMITTED_ROW_TEXT: &str = "…";

/// One style-homogeneous prompt run after visual wrapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptVisualRun {
    text: String,
    selected: bool,
}

impl PromptVisualRun {
    /// Returns run text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Returns whether the run belongs to the current selection.
    pub(crate) fn selected(&self) -> bool {
        self.selected
    }
}

/// One soft-wrapped prompt line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptVisualLine {
    runs: Vec<PromptVisualRun>,
    stops: Vec<CursorStop>,
    end_byte: usize,
    width: usize,
}

impl PromptVisualLine {
    /// Returns visual runs.
    pub(crate) fn runs(&self) -> &[PromptVisualRun] {
        &self.runs
    }
}

/// Cell geometry needed before the visible prompt rows are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptLayoutMetrics {
    /// Total canonical visual rows.
    pub(crate) line_count: usize,
    /// Absolute canonical row containing the cursor.
    pub(crate) cursor_row: usize,
    /// Cursor cell within its canonical row.
    pub(crate) cursor_cell: usize,
}

/// Visible rows derived from one canonical prompt layout.
#[derive(Debug, Clone)]
pub(crate) struct PromptViewport {
    lines: Vec<PromptVisualLine>,
    revision: u64,
    cursor: Option<(usize, usize)>,
}

impl PromptViewport {
    /// Returns visible visual rows.
    pub(crate) fn lines(&self) -> &[PromptVisualLine] {
        &self.lines
    }

    /// Resolves a viewport-relative point to the nearest grapheme boundary.
    pub(crate) fn position_at(&self, row: usize, cell: usize) -> PromptPosition {
        let line = &self.lines[row.min(self.lines.len().saturating_sub(1))];
        let byte = line
            .stops
            .iter()
            .min_by_key(|stop| stop.cell.abs_diff(cell))
            .map_or(line.end_byte, |stop| stop.byte);
        PromptPosition {
            revision: self.revision,
            byte,
        }
    }

    /// Returns the cursor position in bounded viewport coordinates.
    pub(crate) fn cursor_position(&self) -> Option<(usize, usize)> {
        self.cursor
    }

    /// Returns visible projection metrics for the display validation pipeline.
    pub(crate) fn projection_metrics(&self) -> (usize, usize, usize) {
        let bytes = self
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .map(|run| run.text.len())
            .sum();
        let cells = self.lines.iter().map(|line| line.width).sum();
        (bytes, self.lines.len(), cells)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorStop {
    byte: usize,
    cell: usize,
}

/// Sparse, reconstructible index of canonical prompt rows.
#[derive(Debug, Clone)]
pub(crate) struct PromptLayout {
    width: u16,
    line_count: usize,
    checkpoints: Vec<RowCheckpoint>,
    revision: u64,
    cursor_geometry: Cell<Option<CachedCursorGeometry>>,
}

impl PromptLayout {
    /// Builds a sparse row index at the provided content width.
    pub(crate) fn new(text: &str, width: u16, revision: u64) -> Self {
        let mut checkpoints = Vec::new();
        let mut line_count = 0usize;
        scan_rows(
            text,
            width,
            RowCheckpoint::document_start(),
            |row_index, row| {
                if row_index % ROW_CHECKPOINT_INTERVAL == 0 {
                    checkpoints.push(RowCheckpoint::from_row(row_index, row));
                }
                line_count = row_index.saturating_add(1);
                true
            },
        );
        debug_assert!(line_count > 0);
        debug_assert!(!checkpoints.is_empty());
        Self {
            width,
            line_count,
            checkpoints,
            revision,
            cursor_geometry: Cell::new(None),
        }
    }

    /// Returns whether this index matches the current source revision and width.
    pub(crate) fn matches(&self, width: u16, revision: u64) -> bool {
        self.width == width && self.revision == revision
    }

    /// Returns total visual rows and the cursor's absolute cell position.
    pub(crate) fn metrics(&self, text: &str, cursor: usize) -> PromptLayoutMetrics {
        let geometry = self.cursor_geometry(text, cursor);
        PromptLayoutMetrics {
            line_count: self.line_count,
            cursor_row: geometry.row,
            cursor_cell: geometry.cell,
        }
    }

    /// Builds only the canonical rows visible in one prompt viewport.
    pub(crate) fn viewport(
        &self,
        text: &str,
        selection: Option<Range<usize>>,
        cursor: usize,
        first_row: usize,
        height: usize,
    ) -> PromptViewport {
        let first_row = first_row.min(self.line_count.saturating_sub(1));
        let final_row = first_row.saturating_add(height.max(1)).min(self.line_count);
        let checkpoint = self.checkpoint_for_row(first_row);
        let cursor_row = self.cursor_geometry(text, cursor).row;
        let mut cursor_position = None;
        let mut budget = ViewportBudget {
            projection_bytes: MAX_VIEWPORT_PROJECTION_BYTES,
            interior_stops: MAX_VIEWPORT_INTERIOR_STOPS,
        };
        let mut lines = Vec::with_capacity(final_row.saturating_sub(first_row));
        scan_rows(text, self.width, checkpoint, |row_index, row| {
            if row_index >= final_row {
                return false;
            }
            if row_index >= first_row {
                let viewport_row = lines.len();
                let (line, cursor_cell) = build_visual_line(
                    text,
                    row,
                    selection.as_ref(),
                    (row_index == cursor_row).then_some(cursor),
                    &mut budget,
                );
                if let Some(cursor_cell) = cursor_cell {
                    cursor_position = Some((viewport_row, cursor_cell));
                }
                lines.push(line);
            }
            true
        });
        debug_assert!(!lines.is_empty());
        PromptViewport {
            lines,
            revision: self.revision,
            cursor: cursor_position,
        }
    }

    fn cursor_geometry(&self, text: &str, cursor: usize) -> CachedCursorGeometry {
        if let Some(geometry) = self.cursor_geometry.get()
            && geometry.byte == cursor
        {
            return geometry;
        }
        let (row, cell) = self.position_for_byte(text, cursor);
        let geometry = CachedCursorGeometry {
            byte: cursor,
            row,
            cell,
        };
        self.cursor_geometry.set(Some(geometry));
        geometry
    }

    /// Resolves one absolute visual row and cell to a source boundary.
    pub(crate) fn position_at(&self, text: &str, row: usize, cell: usize) -> PromptPosition {
        let row = row.min(self.line_count.saturating_sub(1));
        let checkpoint = self.checkpoint_for_row(row);
        let mut position = None;
        scan_rows(text, self.width, checkpoint, |row_index, geometry| {
            if row_index == row {
                position = Some(position_in_row(text, geometry, cell, self.revision));
                return false;
            }
            true
        });
        position.expect("indexed prompt row resolves from its checkpoint")
    }

    fn position_for_byte(&self, text: &str, byte: usize) -> (usize, usize) {
        debug_assert!(byte <= text.len());
        let checkpoint = self.checkpoint_for_byte(byte);
        let mut position = None;
        scan_rows(text, self.width, checkpoint, |row_index, row| {
            if byte == row.cursor_start_byte {
                position = Some((row_index, 0));
                return false;
            }
            if row.start_byte > byte {
                return false;
            }
            if byte <= row.end_byte {
                position = Some((row_index, cell_at_byte(text, row, byte)));
                if byte < row.end_byte {
                    return false;
                }
            }
            true
        });
        position.expect("grapheme boundary belongs to one indexed prompt row")
    }

    fn checkpoint_for_row(&self, row: usize) -> RowCheckpoint {
        self.checkpoints[row / ROW_CHECKPOINT_INTERVAL]
    }

    fn checkpoint_for_byte(&self, byte: usize) -> RowCheckpoint {
        let index = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.source_byte <= byte)
            .saturating_sub(1);
        self.checkpoints[index]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedCursorGeometry {
    byte: usize,
    row: usize,
    cell: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowCheckpoint {
    row: usize,
    source_byte: usize,
    cursor_start_byte: usize,
    origin: LineOrigin,
}

impl RowCheckpoint {
    fn document_start() -> Self {
        Self {
            row: 0,
            source_byte: 0,
            cursor_start_byte: 0,
            origin: LineOrigin::DocumentStart,
        }
    }

    fn from_row(row: usize, geometry: RowGeometry) -> Self {
        Self {
            row,
            source_byte: geometry.start_byte,
            cursor_start_byte: geometry.cursor_start_byte,
            origin: geometry.origin,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowGeometry {
    origin: LineOrigin,
    cursor_start_byte: usize,
    start_byte: usize,
    end_byte: usize,
    width: usize,
}

impl RowGeometry {
    fn new(source_byte: usize, origin: LineOrigin) -> Self {
        Self {
            origin,
            start_byte: source_byte,
            cursor_start_byte: source_byte,
            end_byte: source_byte,
            width: 0,
        }
    }

    fn is_empty(self) -> bool {
        self.start_byte == self.end_byte
    }
}

/// Reason a visual prompt line begins at its source byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineOrigin {
    /// The line begins at the start of the prompt.
    DocumentStart,
    /// The line begins because content reaches or exceeds the visual width.
    SoftWrap,
    /// The line begins after an explicit line feed.
    HardBreak,
}

struct RowScanner<'a, Visitor> {
    width: usize,
    row_index: usize,
    current: RowGeometry,
    visitor: &'a mut Visitor,
    stopped: bool,
}

impl<Visitor> RowScanner<'_, Visitor>
where
    Visitor: FnMut(usize, RowGeometry) -> bool,
{
    fn push_source_grapheme(&mut self, source_byte: usize, source_len: usize, grapheme: &str) {
        let grapheme_width = projection::display_width(grapheme);
        if self.current.width > 0 && self.current.width.saturating_add(grapheme_width) > self.width
        {
            if !self.emit_current() {
                return;
            }
            self.current = RowGeometry::new(source_byte, LineOrigin::SoftWrap);
        }

        self.current.width = self.current.width.saturating_add(grapheme_width);
        self.current.end_byte = source_byte.saturating_add(source_len);
        if self.current.width >= self.width && self.emit_current() {
            self.current =
                RowGeometry::new(source_byte.saturating_add(source_len), LineOrigin::SoftWrap);
        }
    }

    fn hard_break(&mut self, line_feed_byte: usize, next_source_byte: usize) {
        if self.current.origin == LineOrigin::SoftWrap && self.current.is_empty() {
            let cursor_start_byte = self.current.cursor_start_byte;
            self.current = RowGeometry::new(next_source_byte, LineOrigin::HardBreak);
            self.current.cursor_start_byte = cursor_start_byte;
            return;
        }

        self.current.end_byte = line_feed_byte;
        if self.emit_current() {
            self.current = RowGeometry::new(next_source_byte, LineOrigin::HardBreak);
        }
    }

    fn emit_current(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        let keep_scanning = (self.visitor)(self.row_index, self.current);
        self.row_index = self.row_index.saturating_add(1);
        self.stopped = !keep_scanning;
        keep_scanning
    }
}

fn scan_rows(
    text: &str,
    width: u16,
    checkpoint: RowCheckpoint,
    mut visitor: impl FnMut(usize, RowGeometry) -> bool,
) {
    debug_assert!(text.is_char_boundary(checkpoint.source_byte));
    debug_assert!(text.is_char_boundary(checkpoint.cursor_start_byte));
    debug_assert!(checkpoint.cursor_start_byte <= checkpoint.source_byte);
    let mut current = RowGeometry::new(checkpoint.source_byte, checkpoint.origin);
    current.cursor_start_byte = checkpoint.cursor_start_byte;
    let mut scanner = RowScanner {
        width: usize::from(width.max(1)),
        row_index: checkpoint.row,
        current,
        visitor: &mut visitor,
        stopped: false,
    };

    for (relative_byte, grapheme) in text[checkpoint.source_byte..].grapheme_indices(true) {
        let source_byte = checkpoint.source_byte.saturating_add(relative_byte);
        if let Some(before_line_feed) = grapheme.strip_suffix('\n') {
            if !before_line_feed.is_empty() {
                scanner.push_source_grapheme(source_byte, grapheme.len(), before_line_feed);
                if scanner.stopped {
                    break;
                }
            }
            let next_source_byte = source_byte.saturating_add(grapheme.len());
            scanner.hard_break(
                if before_line_feed.is_empty() {
                    source_byte
                } else {
                    next_source_byte
                },
                next_source_byte,
            );
        } else {
            scanner.push_source_grapheme(source_byte, grapheme.len(), grapheme);
        }
        if scanner.stopped {
            break;
        }
    }

    if !scanner.stopped {
        scanner.emit_current();
    }
}

struct ViewportBudget {
    projection_bytes: usize,
    interior_stops: usize,
}

fn build_visual_line(
    text: &str,
    row: RowGeometry,
    selection: Option<&Range<usize>>,
    cursor: Option<usize>,
    budget: &mut ViewportBudget,
) -> (PromptVisualLine, Option<usize>) {
    let mut runs = Vec::<PromptVisualRun>::new();
    let mut stops = vec![CursorStop {
        byte: row.start_byte,
        cell: 0,
    }];
    let mut rendered_width = 0usize;
    let mut cursor_cell = cursor
        .is_some_and(|cursor| cursor == row.cursor_start_byte || cursor == row.start_byte)
        .then_some(0);
    for (relative_byte, grapheme) in text[row.start_byte..row.end_byte].grapheme_indices(true) {
        let source_byte = row.start_byte.saturating_add(relative_byte);
        let source_end = source_byte.saturating_add(grapheme.len());
        if budget.interior_stops > 0 {
            let display = projection::display_grapheme(grapheme);
            if display.len() <= budget.projection_bytes {
                let selected = selection
                    .is_some_and(|range| range.start <= source_byte && source_byte < range.end);
                push_visual_run(&mut runs, &display, selected);
                budget.projection_bytes -= display.len();
                budget.interior_stops -= 1;
                rendered_width = rendered_width.saturating_add(display.width());
                stops.push(CursorStop {
                    byte: source_end,
                    cell: rendered_width,
                });
                if cursor == Some(source_end) {
                    cursor_cell = Some(rendered_width);
                }
                continue;
            }
        }

        let omitted_selected =
            selection.is_some_and(|range| range.start < row.end_byte && source_byte < range.end);
        let marker_start = rendered_width;
        push_visual_run(&mut runs, OMITTED_ROW_TEXT, omitted_selected);
        rendered_width = rendered_width.saturating_add(OMITTED_ROW_TEXT.width());
        if stops.last().is_none_or(|stop| stop.byte != source_byte) {
            stops.push(CursorStop {
                byte: source_byte,
                cell: marker_start,
            });
        }
        stops.push(CursorStop {
            byte: row.end_byte,
            cell: rendered_width,
        });
        if cursor.is_some_and(|cursor| source_byte < cursor && cursor <= row.end_byte) {
            cursor_cell = Some(rendered_width);
        } else if cursor == Some(source_byte) {
            cursor_cell = Some(marker_start);
        }
        break;
    }
    if stops.last().is_none_or(|stop| stop.byte != row.end_byte) {
        stops.push(CursorStop {
            byte: row.end_byte,
            cell: rendered_width,
        });
    }
    (
        PromptVisualLine {
            runs,
            stops,
            end_byte: row.end_byte,
            width: rendered_width,
        },
        cursor_cell,
    )
}

fn push_visual_run(runs: &mut Vec<PromptVisualRun>, text: &str, selected: bool) {
    if let Some(last) = runs.last_mut()
        && last.selected == selected
    {
        last.text.push_str(text);
    } else {
        runs.push(PromptVisualRun {
            text: text.to_string(),
            selected,
        });
    }
}

fn position_in_row(text: &str, row: RowGeometry, cell: usize, revision: u64) -> PromptPosition {
    let mut best = CursorStop {
        byte: row.start_byte,
        cell: 0,
    };
    let mut width = 0usize;
    for (relative_byte, grapheme) in text[row.start_byte..row.end_byte].grapheme_indices(true) {
        width = width.saturating_add(projection::display_width(grapheme));
        let candidate = CursorStop {
            byte: row
                .start_byte
                .saturating_add(relative_byte)
                .saturating_add(grapheme.len()),
            cell: width,
        };
        if candidate.cell.abs_diff(cell) < best.cell.abs_diff(cell) {
            best = candidate;
        }
    }
    PromptPosition {
        revision,
        byte: best.byte,
    }
}

fn cell_at_byte(text: &str, row: RowGeometry, byte: usize) -> usize {
    debug_assert!(row.start_byte <= byte && byte <= row.end_byte);
    text[row.start_byte..byte]
        .graphemes(true)
        .map(|grapheme| projection::display_width(grapheme))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(
        text: &str,
        cursor: usize,
        width: u16,
    ) -> (PromptLayout, PromptLayoutMetrics, PromptViewport) {
        let layout = PromptLayout::new(text, width, 0);
        let metrics = layout.metrics(text, cursor);
        let viewport = layout.viewport(text, None, cursor, 0, metrics.line_count);
        (layout, metrics, viewport)
    }

    #[test]
    fn layout_uses_cells_and_graphemes_for_every_mapping() {
        let text = "a好e\u{301}x";
        let cursor = "a好e\u{301}".len();
        let (layout, metrics, _) = prepared(text, cursor, 4);

        assert_eq!(metrics.line_count, 2);
        assert_eq!((metrics.cursor_row, metrics.cursor_cell), (1, 0));
        assert_eq!(layout.position_at(text, 0, 2).byte, 1);
        assert_eq!(layout.position_at(text, 1, 0).byte, cursor);
        assert_eq!(layout.position_at(text, 1, 1).byte, text.len());
    }
    #[test]
    fn viewport_reports_cursor_in_rendered_projection_coordinates() {
        let text = "a好b";
        let cursor = "a好".len();
        let layout = PromptLayout::new(text, 10, 0);
        let viewport = layout.viewport(text, None, cursor, 0, 1);

        assert_eq!(viewport.cursor_position(), Some((0, 3)));
    }

    #[test]
    fn exact_width_places_caret_on_following_visual_row() {
        let (_, metrics, _) = prepared("abcd", 4, 4);

        assert_eq!((metrics.cursor_row, metrics.cursor_cell), (1, 0));
        assert_eq!(metrics.line_count, 2);
    }

    #[test]
    fn exact_width_followed_by_hard_break_uses_the_wrapped_row() {
        let text = "abcd\nx";
        let (layout, metrics, viewport) = prepared(text, text.len(), 4);

        assert_eq!(metrics.line_count, 2);
        assert_eq!(viewport.lines()[0].runs()[0].text(), "abcd");
        assert_eq!(viewport.lines()[1].runs()[0].text(), "x");
        assert_eq!((metrics.cursor_row, metrics.cursor_cell), (1, 1));
        assert_eq!(layout.position_at(text, 1, 0).byte, "abcd\n".len());
    }

    #[test]
    fn cursor_before_hard_break_keeps_the_reused_soft_wrap_row() {
        let text = "abcd\nx";
        let layout = PromptLayout::new(text, 4, 0);
        let before_line_feed = layout.metrics(text, 4);
        let after_line_feed = layout.metrics(text, 5);
        let before_viewport = layout.viewport(text, None, 4, 1, 1);
        let after_viewport = layout.viewport(text, None, 5, 1, 1);

        assert_eq!(
            (before_line_feed.cursor_row, before_line_feed.cursor_cell),
            (1, 0)
        );
        assert_eq!(
            (after_line_feed.cursor_row, after_line_feed.cursor_cell),
            (1, 0)
        );
        assert_eq!(before_viewport.cursor_position(), Some((0, 0)));
        assert_eq!(after_viewport.cursor_position(), Some((0, 0)));
        assert_eq!(layout.position_at(text, 1, 0).byte, 5);
    }

    #[test]
    fn sparse_checkpoint_preserves_reused_row_cursor_boundaries() {
        let text = "abcd\n".repeat(ROW_CHECKPOINT_INTERVAL + 2);
        let layout = PromptLayout::new(&text, 4, 0);
        let row = ROW_CHECKPOINT_INTERVAL;
        let before_line_feed = row.saturating_mul(5).saturating_sub(1);
        let after_line_feed = before_line_feed.saturating_add(1);
        let before_viewport = layout.viewport(&text, None, before_line_feed, row, 1);
        let after_viewport = layout.viewport(&text, None, after_line_feed, row, 1);

        assert_eq!(
            layout.metrics(&text, before_line_feed),
            PromptLayoutMetrics {
                line_count: ROW_CHECKPOINT_INTERVAL + 3,
                cursor_row: row,
                cursor_cell: 0,
            }
        );
        assert_eq!(
            layout.metrics(&text, after_line_feed),
            PromptLayoutMetrics {
                line_count: ROW_CHECKPOINT_INTERVAL + 3,
                cursor_row: row,
                cursor_cell: 0,
            }
        );
        assert_eq!(before_viewport.cursor_position(), Some((0, 0)));
        assert_eq!(after_viewport.cursor_position(), Some((0, 0)));
        assert_eq!(before_viewport.lines()[0].runs()[0].text(), "abcd");
    }

    #[test]
    fn crlf_projects_carriage_return_and_preserves_the_line_boundary() {
        let text = "a\r\nb";
        let (layout, metrics, viewport) = prepared(text, text.len(), 10);

        assert_eq!(metrics.line_count, 2);
        assert_eq!(viewport.lines()[0].runs()[0].text(), "a␍");
        assert_eq!(viewport.lines()[1].runs()[0].text(), "b");
        assert_eq!((metrics.cursor_row, metrics.cursor_cell), (1, 1));
        assert_eq!(layout.position_at(text, 0, 2).byte, "a\r\n".len());
        assert_eq!(layout.position_at(text, 1, 0).byte, "a\r\n".len());
    }

    #[test]
    fn layout_projects_terminal_controls_but_maps_original_bytes() {
        let text = "\u{1b}[31m";
        let (layout, metrics, viewport) = prepared(text, text.len(), 20);

        assert_eq!(viewport.lines()[0].runs()[0].text(), "␛[31m");
        assert_eq!((metrics.cursor_row, metrics.cursor_cell), (0, 5));
        assert_eq!(layout.position_at(text, 0, 1).byte, '\u{1b}'.len_utf8());
    }

    #[test]
    fn sparse_checkpoints_resolve_rows_and_bytes_beyond_one_interval() {
        let text = (0..300)
            .map(|index| format!("{index:03}\r\n"))
            .collect::<String>();
        let cursor = text.find("200").unwrap();
        let layout = PromptLayout::new(&text, 4, 9);
        let metrics = layout.metrics(&text, cursor);
        let viewport = layout.viewport(&text, None, cursor, 198, 5);

        assert_eq!(metrics.cursor_row, 200);
        assert_eq!(layout.position_at(&text, 200, 0).byte, cursor);
        assert_eq!(viewport.lines()[2].runs()[0].text(), "200␍");
    }

    #[test]
    fn viewport_projection_allocates_only_requested_rows() {
        let text = (0..10_000)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        let layout = PromptLayout::new(&text, 20, 0);
        let viewport = layout.viewport(
            &text,
            None,
            text.len(),
            layout.line_count.saturating_sub(3),
            3,
        );
        let (bytes, lines, cells) = viewport.projection_metrics();

        assert_eq!(lines, 3);
        assert!(bytes < 64);
        assert!(cells < 64);
        assert!(viewport.lines()[2].runs().is_empty());
    }
    #[test]
    fn viewport_projection_has_a_total_byte_and_hit_map_budget() {
        let grapheme = format!("a{}", "\u{301}".repeat(512));
        let text = (0..400).map(|_| grapheme.as_str()).collect::<String>();
        let layout = PromptLayout::new(&text, 1, 0);
        let viewport = layout.viewport(&text, None, text.len(), 0, layout.line_count);
        let (bytes, lines, _) = viewport.projection_metrics();

        assert_eq!(lines, 401);
        assert!(
            bytes <= MAX_VIEWPORT_PROJECTION_BYTES + OMITTED_ROW_TEXT.len().saturating_mul(lines)
        );
        assert!(
            viewport
                .lines()
                .iter()
                .flat_map(PromptVisualLine::runs)
                .any(|run| run.text().contains(OMITTED_ROW_TEXT))
        );
        assert_eq!(viewport.position_at(lines - 1, usize::MAX).byte, text.len());
    }
    #[test]
    fn bounded_projection_keeps_cursor_hit_and_selection_coordinates_consistent() {
        let text = "a".repeat(70_000);
        let layout = PromptLayout::new(&text, 1_000, 0);
        let cursor = 65_700;
        let selection = 65_600..cursor;
        let viewport = layout.viewport(&text, Some(selection), cursor, 0, layout.line_count);
        let row = &viewport.lines()[65];
        let marker_index = row
            .runs()
            .iter()
            .position(|run| run.text().contains(OMITTED_ROW_TEXT))
            .expect("bounded projection contains an omission marker");
        let marker_run = &row.runs()[marker_index];
        let marker_start = row.runs()[..marker_index]
            .iter()
            .map(|run| run.text().width())
            .sum::<usize>();
        let marker_end = marker_start.saturating_add(OMITTED_ROW_TEXT.width());
        let rendered_width = row
            .runs()
            .iter()
            .map(|run| run.text().width())
            .sum::<usize>();

        assert!(marker_run.selected());
        assert_eq!(rendered_width, row.width);
        assert_eq!(viewport.cursor_position(), Some((65, marker_end)));
        assert_eq!(viewport.position_at(65, marker_start).byte, 65_536);
        assert_eq!(viewport.position_at(65, marker_end).byte, 66_000);
    }
}
