//! Cell-width-aware wrapping for validated display documents.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    DocumentLimits, DocumentLine, DocumentRun, LaidOut, StyleId,
    is_permitted_display_character,
};

pub(super) const OMISSION_MARKER: &str = "… output truncated …";
pub(super) const OMISSION_MARKER_BYTES: usize = OMISSION_MARKER.len();
pub(super) const OMISSION_MARKER_CELLS: usize = 20;
const ONE_LINE_OMISSION_MARKER: &str = "…";
const ONE_LINE_OMISSION_MARKER_BYTES: usize = ONE_LINE_OMISSION_MARKER.len();
const ONE_LINE_OMISSION_MARKER_CELLS: usize = 1;

/// One selectable range associated with an occupied cell or zero-width boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CellHit {
    /// Half-open terminal-cell range occupied by the grapheme.
    ///
    /// Zero-width graphemes use an empty range and participate only in nearest
    /// boundary resolution; they never claim a terminal cell.
    pub(crate) cells: Range<usize>,
    /// Half-open byte range in the document's selectable text.
    pub(crate) selection_bytes: Option<Range<usize>>,
}

/// One validated display run after wrapping.
#[derive(Debug, Clone)]
pub(crate) struct LaidOutRun {
    text: String,
    style: StyleId,
    selection_bytes: Option<Range<usize>>,
}

impl LaidOutRun {
    /// Returns the run text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Returns the semantic style.
    pub(crate) fn style(&self) -> StyleId {
        self.style
    }

    /// Returns the selectable byte range represented by this run.
    pub(crate) fn selection_bytes(&self) -> Option<Range<usize>> {
        self.selection_bytes.clone()
    }
}

/// One terminal-cell-wrapped line.
#[derive(Debug, Clone)]
pub(crate) struct LaidOutLine {
    runs: Vec<LaidOutRun>,
    hits: Vec<CellHit>,
    width: usize,
    projection_bytes: Range<usize>,
}

impl LaidOutLine {
    /// Returns display runs in terminal order.
    pub(crate) fn runs(&self) -> &[LaidOutRun] {
        &self.runs
    }

    /// Returns the occupied terminal-cell width.
    pub(crate) fn width(&self) -> usize {
        self.width
    }

    /// Returns the half-open byte range in the complete visible projection.
    ///
    /// Unlike selectable run ranges, this range includes visual decorations.
    /// Adjacent soft-wrapped rows share an end/start boundary.
    pub(crate) fn projection_bytes(&self) -> Range<usize> {
        self.projection_bytes.clone()
    }

    /// Resolves a terminal cell to the nearest selectable byte position.
    pub(crate) fn selection_position(&self, cell: usize) -> Option<usize> {
        let mut selectable_hits = self.hits.iter().filter(|hit| hit.selection_bytes.is_some());
        let hit = selectable_hits
            .clone()
            .find(|hit| hit.cells.contains(&cell))
            .or_else(|| {
                selectable_hits
                    .clone()
                    .rev()
                    .find(|hit| hit.cells.end <= cell)
            })
            .or_else(|| selectable_hits.find(|hit| hit.cells.start >= cell))?;
        let range = hit
            .selection_bytes
            .as_ref()
            .expect("selectable hit contains a byte range");
        if cell >= hit.cells.end {
            Some(range.end)
        } else {
            Some(range.start)
        }
    }
}

pub(super) fn bound_lines(lines: Vec<DocumentLine>, limits: DocumentLimits) -> Vec<DocumentLine> {
    let source_metrics = document_metrics(&lines);
    if source_metrics.bytes <= limits.max_bytes
        && source_metrics.lines <= limits.max_lines
    {
        return ensure_nonempty(lines);
    }

    let source_byte_limit = limits.max_bytes.saturating_sub(OMISSION_MARKER_BYTES);
    let source_line_limit = limits.max_lines.saturating_sub(1);
    let mut output = Vec::new();
    let mut bytes = 0usize;

    'lines: for line in lines.into_iter().take(source_line_limit) {
        let mut bounded = DocumentLine { runs: Vec::new() };
        for run in line.runs {
            let mut text = String::new();
            for grapheme in run.text.graphemes(true) {
                let next_bytes = bytes.saturating_add(grapheme.len());
                if next_bytes > source_byte_limit {
                    if !text.is_empty() {
                        bounded.runs.push(DocumentRun {
                            text,
                            style: run.style,
                            selectable: run.selectable,
                        });
                    }
                    output.push(bounded);
                    break 'lines;
                }
                text.push_str(grapheme);
                bytes = next_bytes;
            }
            if !text.is_empty() {
                bounded.runs.push(DocumentRun {
                    text,
                    style: run.style,
                    selectable: run.selectable,
                });
            }
        }
        output.push(bounded);
    }

    output.push(DocumentLine {
        runs: vec![DocumentRun {
            text: OMISSION_MARKER.to_string(),
            style: StyleId::Muted,
            selectable: false,
        }],
    });

    let bounded_metrics = document_metrics(&output);
    debug_assert!(bounded_metrics.bytes <= limits.max_bytes);
    debug_assert!(bounded_metrics.lines <= limits.max_lines);
    debug_assert!(output.iter().all(|line| {
        line.runs
            .iter()
            .all(|run| run.text.chars().all(is_permitted_display_character))
    }));
    output
}
pub(super) fn bound_one_line(
    lines: Vec<DocumentLine>,
    limits: DocumentLimits,
    width: usize,
) -> Vec<DocumentLine> {
    let lines = ensure_nonempty(lines);
    let source_metrics = document_metrics(&lines);
    if lines.len() == 1
        && source_metrics.bytes <= limits.max_bytes
        && source_metrics.cells <= width
    {
        return lines;
    }
    if width == 0 {
        return vec![DocumentLine { runs: Vec::new() }];
    }

    let source_byte_limit = limits
        .max_bytes
        .saturating_sub(ONE_LINE_OMISSION_MARKER_BYTES);
    let source_cell_limit = width.saturating_sub(ONE_LINE_OMISSION_MARKER_CELLS);
    let mut output = DocumentLine { runs: Vec::new() };
    let mut bytes = 0usize;
    let mut cells = 0usize;
    if let Some(first_line) = lines.into_iter().next() {
        'runs: for run in first_line.runs {
            let mut text = String::new();
            for grapheme in run.text.graphemes(true) {
                let next_bytes = bytes.saturating_add(grapheme.len());
                let next_cells = cells.saturating_add(grapheme.width());
                if next_bytes > source_byte_limit || next_cells > source_cell_limit {
                    if !text.is_empty() {
                        output.runs.push(DocumentRun {
                            text,
                            style: run.style,
                            selectable: run.selectable,
                        });
                    }
                    break 'runs;
                }
                text.push_str(grapheme);
                bytes = next_bytes;
                cells = next_cells;
            }
            if !text.is_empty() {
                output.runs.push(DocumentRun {
                    text,
                    style: run.style,
                    selectable: run.selectable,
                });
            }
        }
    }
    output.runs.push(DocumentRun {
        text: ONE_LINE_OMISSION_MARKER.to_string(),
        style: StyleId::Muted,
        selectable: false,
    });

    let output = vec![output];
    let bounded_metrics = document_metrics(&output);
    debug_assert!(bounded_metrics.bytes <= limits.max_bytes);
    debug_assert!(bounded_metrics.lines == 1);
    debug_assert!(bounded_metrics.cells <= width);
    output
}
#[derive(Debug, Clone, Copy)]
struct DocumentMetrics {
    bytes: usize,
    lines: usize,
    cells: usize,
}

fn document_metrics(lines: &[DocumentLine]) -> DocumentMetrics {
    DocumentMetrics {
        bytes: lines
            .iter()
            .flat_map(|line| &line.runs)
            .map(|run| run.text.len())
            .sum(),
        lines: lines.len(),
        cells: lines
            .iter()
            .flat_map(|line| &line.runs)
            .map(|run| run.text.width())
            .sum(),
    }
}

fn ensure_nonempty(mut lines: Vec<DocumentLine>) -> Vec<DocumentLine> {
    if lines.is_empty() {
        lines.push(DocumentLine { runs: Vec::new() });
    }
    lines
}

pub(super) fn layout_lines(lines: Vec<DocumentLine>, width: usize) -> LaidOut {
    let source_line_count = lines.len();
    let mut laid_out = Vec::new();
    let mut selection_cursor = 0usize;
    let mut projection_cursor = 0usize;

    for (line_index, line) in lines.into_iter().enumerate() {
        let source_line_is_empty = line.runs.is_empty();
        let mut current = LineBuilder::new(projection_cursor);
        for run in line.runs {
            for grapheme in run.text.graphemes(true) {
                let grapheme_width = grapheme.width();
                if current.width > 0 && current.width.saturating_add(grapheme_width) > width {
                    laid_out.push(current.finish(projection_cursor));
                    current = LineBuilder::new(projection_cursor);
                }

                let selection_bytes = if run.selectable {
                    let range = selection_cursor..selection_cursor.saturating_add(grapheme.len());
                    selection_cursor = range.end;
                    Some(range)
                } else {
                    None
                };
                current.push(grapheme, run.style, grapheme_width, selection_bytes);
                projection_cursor = projection_cursor.saturating_add(grapheme.len());

                if current.width >= width {
                    laid_out.push(current.finish(projection_cursor));
                    current = LineBuilder::new(projection_cursor);
                }
            }
        }
        if !current.runs.is_empty() || source_line_is_empty {
            laid_out.push(current.finish(projection_cursor));
        }
        if line_index + 1 < source_line_count {
            selection_cursor = selection_cursor.saturating_add(1);
            projection_cursor = projection_cursor.saturating_add(1);
        }
    }

    if laid_out.is_empty() {
        laid_out.push(LineBuilder::new(projection_cursor).finish(projection_cursor));
    }

    LaidOut { lines: laid_out }
}

#[derive(Debug)]
struct LineBuilder {
    runs: Vec<LaidOutRun>,
    hits: Vec<CellHit>,
    width: usize,
    projection_start: usize,
}

impl LineBuilder {
    fn new(projection_start: usize) -> Self {
        Self {
            runs: Vec::new(),
            hits: Vec::new(),
            width: 0,
            projection_start,
        }
    }

    fn push(
        &mut self,
        grapheme: &str,
        style: StyleId,
        grapheme_width: usize,
        selection_bytes: Option<Range<usize>>,
    ) {
        let start_cell = self.width;
        self.width = self.width.saturating_add(grapheme_width);
        self.hits.push(CellHit {
            cells: start_cell..self.width,
            selection_bytes: selection_bytes.clone(),
        });

        if let Some(last) = self.runs.last_mut()
            && last.style == style
            && ranges_are_contiguous(last.selection_bytes.as_ref(), selection_bytes.as_ref())
        {
            last.text.push_str(grapheme);
            if let (Some(last_range), Some(next_range)) =
                (&mut last.selection_bytes, selection_bytes)
            {
                last_range.end = next_range.end;
            }
            return;
        }

        self.runs.push(LaidOutRun {
            text: grapheme.to_string(),
            style,
            selection_bytes,
        });
    }

    fn finish(self, projection_end: usize) -> LaidOutLine {
        debug_assert!(self.projection_start <= projection_end);
        LaidOutLine {
            runs: self.runs,
            hits: self.hits,
            width: self.width,
            projection_bytes: self.projection_start..projection_end,
        }
    }
}

fn ranges_are_contiguous(left: Option<&Range<usize>>, right: Option<&Range<usize>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.end == right.start,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{RawDocumentBuilder, StyleId};

    #[test]
    fn layout_wraps_by_grapheme_cell_width() {
        let mut raw = RawDocumentBuilder::new();
        raw.plain("好a", StyleId::Plain, true);
        let laid_out = raw
            .build()
            .parse()
            .sanitize()
            .bound(DocumentLimits::UI_LABEL)
            .layout(2);
        assert_eq!(laid_out.lines().len(), 2);
        assert_eq!(laid_out.lines()[0].runs()[0].text(), "好");
        assert_eq!(laid_out.lines()[1].runs()[0].text(), "a");
    }

    #[test]
    fn layout_keeps_combining_grapheme_together() {
        let mut raw = RawDocumentBuilder::new();
        raw.plain("e\u{301}x", StyleId::Plain, true);
        let laid_out = raw
            .build()
            .parse()
            .sanitize()
            .bound(DocumentLimits::UI_LABEL)
            .layout(1);
        assert_eq!(laid_out.lines().len(), 2);
        assert_eq!(laid_out.lines()[0].runs()[0].text(), "e\u{301}");
    }
    #[test]
    fn zero_width_text_does_not_steal_the_following_cell() {
        let mut raw = RawDocumentBuilder::new();
        raw.plain("\u{301}a", StyleId::Plain, true);
        let laid_out = raw
            .build()
            .parse()
            .sanitize()
            .bound(DocumentLimits::UI_LABEL)
            .layout(1);
        let line = &laid_out.lines()[0];

        assert_eq!(line.width(), 1);
        assert_eq!(line.selection_position(0), Some("\u{301}".len()));
    }

    #[test]
    fn truncation_marker_stays_inside_every_bound() {
        let limits = DocumentLimits::new(OMISSION_MARKER_BYTES + 3, 2);
        let lines = vec![
            DocumentLine {
                runs: vec![DocumentRun {
                    text: "abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
                    style: StyleId::Plain,
                    selectable: true,
                }],
            },
            DocumentLine {
                runs: vec![DocumentRun {
                    text: "second".to_string(),
                    style: StyleId::Plain,
                    selectable: true,
                }],
            },
        ];

        let bounded = bound_lines(lines, limits);
        let metrics = document_metrics(&bounded);
        assert_eq!(metrics.lines, 2);
        assert!(metrics.bytes <= limits.max_bytes);
        assert_eq!(bounded[0].runs[0].text, "abc");
        assert_eq!(bounded[1].runs[0].text, OMISSION_MARKER);
    }
    #[test]
    fn one_line_bound_truncates_before_layout_and_reserves_the_marker() {
        let mut raw = RawDocumentBuilder::new();
        raw.plain("ab好cd\nsecond", StyleId::Plain, true);
        let control_free = raw.build().parse().sanitize();

        let width_zero = control_free
            .clone()
            .bound_one_line(DocumentLimits::UI_LABEL, 0)
            .layout(0);
        assert_eq!(width_zero.lines().len(), 1);
        assert_eq!(width_zero.lines()[0].width(), 0);

        let width_one = control_free
            .clone()
            .bound_one_line(DocumentLimits::UI_LABEL, 1)
            .layout(1);
        assert_eq!(width_one.lines().len(), 1);
        assert_eq!(width_one.lines()[0].runs()[0].text(), "…");

        let width_four = control_free
            .bound_one_line(DocumentLimits::UI_LABEL, 4)
            .layout(4);
        let text = width_four.lines()[0]
            .runs()
            .iter()
            .map(LaidOutRun::text)
            .collect::<String>();
        assert_eq!(text, "ab…");
        assert_eq!(width_four.lines()[0].width(), 3);
    }
}
