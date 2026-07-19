//! Exact prompt ownership, resource bounds, and safe visual projection.

mod fragment;
mod history;
mod layout;
mod projection;

use std::{fmt, ops::Range};

pub(crate) use fragment::{BoundedInput, InputFragment, RawInput};
use history::{EditHistory, EditRecord, EditorState};
pub(crate) use layout::{PromptLayout, PromptLayoutMetrics, PromptViewport};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const MAX_PROMPT_BYTES: usize = 16 * 1024 * 1024;

/// Reachable failure when an edit would exceed prompt storage capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptCapacityError {
    actual_bytes: usize,
    maximum_bytes: usize,
}

#[cfg(test)]
impl PromptCapacityError {
    /// Returns the attempted prompt size.
    pub(crate) fn actual_bytes(self) -> usize {
        self.actual_bytes
    }
}

impl fmt::Display for PromptCapacityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "prompt contains {} UTF-8 bytes; maximum is {}",
            self.actual_bytes, self.maximum_bytes
        )
    }
}

impl std::error::Error for PromptCapacityError {}

/// Failure while importing prompt state from an external snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptImportError {
    /// Imported prompt storage exceeds the supported capacity.
    Capacity(PromptCapacityError),
    /// Imported cursor is not a grapheme boundary in the imported text.
    InvalidCursor { cursor: usize },
}

impl fmt::Display for PromptImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capacity(error) => error.fmt(formatter),
            Self::InvalidCursor { cursor } => {
                write!(
                    formatter,
                    "prompt cursor {cursor} is not a grapheme boundary"
                )
            }
        }
    }
}

impl std::error::Error for PromptImportError {}

impl From<PromptCapacityError> for PromptImportError {
    fn from(error: PromptCapacityError) -> Self {
        Self::Capacity(error)
    }
}

/// Horizontal prompt movement unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HorizontalUnit {
    /// Move by one grapheme cluster.
    Grapheme,
    /// Move by one whitespace-delimited word.
    Word,
}

/// Vertical prompt movement direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerticalDirection {
    /// Move toward earlier visual rows.
    Up,
    /// Move toward later visual rows.
    Down,
}

/// Opaque grapheme boundary produced by one exact prompt layout revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptPosition {
    revision: u64,
    byte: usize,
}

/// Opaque token identifying the exact draft prepared for submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubmissionToken {
    revision: u64,
}

/// Exact prompt submission that has not yet been accepted by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedSubmission {
    text: String,
    token: SubmissionToken,
}

impl PreparedSubmission {
    /// Returns exact user-owned text sent to the runtime.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Returns the token used to commit accepted delivery.
    pub(crate) fn token(&self) -> SubmissionToken {
        self.token
    }
}

/// Exact prompt editor with exclusive ownership of all editing state.
#[derive(Debug, Clone)]
pub(crate) struct PromptEditor {
    text: String,
    cursor: usize,
    selection_anchor: Option<usize>,
    vertical_goal_cell: Option<usize>,
    history: EditHistory,
    revision: u64,
    layout: Option<PromptLayout>,
}

impl PromptEditor {
    /// Imports exact prompt state after validating size and cursor invariants.
    pub(crate) fn import(text: String, cursor: usize) -> Result<Self, PromptImportError> {
        fragment::validate_prompt_size(&text)?;
        if !is_grapheme_boundary(&text, cursor) {
            return Err(PromptImportError::InvalidCursor { cursor });
        }
        Ok(Self {
            text,
            cursor,
            selection_anchor: None,
            vertical_goal_cell: None,
            history: EditHistory::default(),
            revision: 0,
            layout: None,
        })
    }

    #[cfg(test)]
    /// Returns current prompt text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    /// Returns current cursor byte position.
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// Returns the non-empty selected byte range.
    pub(crate) fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.selection_anchor?;
        match anchor.cmp(&self.cursor) {
            std::cmp::Ordering::Less => Some(anchor..self.cursor),
            std::cmp::Ordering::Greater => Some(self.cursor..anchor),
            std::cmp::Ordering::Equal => None,
        }
    }

    /// Returns whether prompt text is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Returns canonical prompt geometry at one content width.
    pub(crate) fn layout_metrics(&mut self, width: u16) -> PromptLayoutMetrics {
        self.ensure_layout(width);
        self.layout
            .as_ref()
            .expect("prompt layout is initialized")
            .metrics(&self.text, self.cursor)
    }

    /// Builds only the visible rows from the canonical prompt layout.
    pub(crate) fn viewport(
        &mut self,
        width: u16,
        first_row: usize,
        height: usize,
    ) -> PromptViewport {
        self.ensure_layout(width);
        self.layout
            .as_ref()
            .expect("prompt layout is initialized")
            .viewport(&self.text, self.selection(), self.cursor, first_row, height)
    }

    /// Inserts an exact resource-bounded fragment as one edit transaction.
    pub(crate) fn insert(
        &mut self,
        fragment: InputFragment<BoundedInput>,
    ) -> Result<(), PromptCapacityError> {
        if fragment.as_str().is_empty() {
            return Ok(());
        }
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        self.replace_range(range, fragment.into_string())
    }

    /// Deletes the current selection or the grapheme before the cursor.
    pub(crate) fn delete_backward(&mut self) {
        let range = self
            .selection()
            .unwrap_or_else(|| previous_grapheme_boundary(&self.text, self.cursor)..self.cursor);
        self.delete_range(range);
    }

    /// Deletes the current selection or the grapheme after the cursor.
    pub(crate) fn delete_forward(&mut self) {
        let range = self
            .selection()
            .unwrap_or_else(|| self.cursor..next_grapheme_boundary(&self.text, self.cursor));
        self.delete_range(range);
    }

    /// Deletes the current selection or the word before the cursor.
    pub(crate) fn delete_word_backward(&mut self) {
        let range = self
            .selection()
            .unwrap_or_else(|| previous_word_boundary(&self.text, self.cursor)..self.cursor);
        self.delete_range(range);
    }

    /// Clears the complete prompt as one undoable transaction.
    pub(crate) fn clear(&mut self) {
        if self.text.is_empty() {
            self.cursor = 0;
            self.clear_selection();
            return;
        }
        self.replace_range(0..self.text.len(), String::new())
            .expect("removing prompt text cannot exceed a resource limit");
    }

    /// Moves horizontally and optionally extends the selection.
    pub(crate) fn move_horizontal(
        &mut self,
        direction: std::cmp::Ordering,
        unit: HorizontalUnit,
        selecting: bool,
    ) {
        let target = match (direction, unit) {
            (std::cmp::Ordering::Less, HorizontalUnit::Grapheme) => {
                previous_grapheme_boundary(&self.text, self.cursor)
            }
            (std::cmp::Ordering::Greater, HorizontalUnit::Grapheme) => {
                next_grapheme_boundary(&self.text, self.cursor)
            }
            (std::cmp::Ordering::Less, HorizontalUnit::Word) => {
                previous_word_boundary(&self.text, self.cursor)
            }
            (std::cmp::Ordering::Greater, HorizontalUnit::Word) => {
                next_word_boundary(&self.text, self.cursor)
            }
            (std::cmp::Ordering::Equal, _) => self.cursor,
        };
        self.vertical_goal_cell = None;
        self.move_cursor(target, selecting);
    }

    /// Moves to a logical line boundary and optionally extends the selection.
    pub(crate) fn move_to_line_boundary(&mut self, end: bool, selecting: bool) {
        let target = if end {
            logical_line_end(&self.text, self.cursor)
        } else {
            logical_line_start(&self.text, self.cursor)
        };
        self.vertical_goal_cell = None;
        self.move_cursor(target, selecting);
    }

    /// Moves through the canonical visual layout.
    pub(crate) fn move_vertical(
        &mut self,
        width: u16,
        direction: VerticalDirection,
        selecting: bool,
    ) {
        self.ensure_layout(width);
        let layout = self.layout.as_ref().expect("prompt layout is initialized");
        let metrics = layout.metrics(&self.text, self.cursor);
        let goal = self.vertical_goal_cell.unwrap_or(metrics.cursor_cell);
        let target_row = match direction {
            VerticalDirection::Up => metrics.cursor_row.checked_sub(1),
            VerticalDirection::Down => {
                let next = metrics.cursor_row.saturating_add(1);
                (next < metrics.line_count).then_some(next)
            }
        };
        let Some(target_row) = target_row else {
            return;
        };
        let target = layout.position_at(&self.text, target_row, goal);
        self.move_to_position(target, selecting);
        self.vertical_goal_cell = Some(goal);
    }

    /// Starts selection at a boundary produced by canonical hit testing.
    pub(crate) fn begin_selection_at(&mut self, target: PromptPosition) {
        self.assert_current_position(target);
        self.cursor = target.byte;
        self.selection_anchor = Some(target.byte);
        self.vertical_goal_cell = None;
    }

    /// Extends selection to a boundary produced by canonical hit testing.
    pub(crate) fn extend_selection_to(&mut self, target: PromptPosition) {
        self.assert_current_position(target);
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
        self.cursor = target.byte;
        self.clear_collapsed_selection();
        self.vertical_goal_cell = None;
    }

    /// Clears prompt selection.
    pub(crate) fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Restores the previous edit transaction.
    pub(crate) fn undo(&mut self) {
        let Some(record) = self.history.pop_undo() else {
            return;
        };
        let range = record.start..record.start.saturating_add(record.inserted.len());
        debug_assert_eq!(&self.text[range.clone()], record.inserted);
        self.text.replace_range(range, &record.removed);
        self.restore_editor_state(record.before);
        self.history.push_redo(record);
        self.bump_revision();
        self.debug_assert_invariants();
    }

    /// Reapplies the next undone transaction.
    pub(crate) fn redo(&mut self) {
        let Some(record) = self.history.pop_redo() else {
            return;
        };
        let range = record.start..record.start.saturating_add(record.removed.len());
        debug_assert_eq!(&self.text[range.clone()], record.removed);
        self.text.replace_range(range, &record.inserted);
        self.restore_editor_state(record.after);
        self.history.push_undo_from_redo(record);
        self.bump_revision();
        self.debug_assert_invariants();
    }

    /// Prepares a non-whitespace submission without changing the draft.
    pub(crate) fn prepare_submission(&self) -> Option<PreparedSubmission> {
        (!self.text.trim().is_empty()).then(|| PreparedSubmission {
            text: self.text.clone(),
            token: SubmissionToken {
                revision: self.revision,
            },
        })
    }

    /// Commits a submission after the runtime accepts its command.
    pub(crate) fn commit_submission(&mut self, token: SubmissionToken) {
        assert_eq!(
            self.revision, token.revision,
            "submitted prompt remains unchanged until delivery completes"
        );
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = None;
        self.vertical_goal_cell = None;
        self.history.clear();
        self.bump_revision();
    }

    /// Consumes the editor and returns snapshot-compatible prompt state.
    pub(crate) fn into_parts(self) -> (String, usize) {
        (self.text, self.cursor)
    }

    fn delete_range(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.replace_range(range, String::new())
            .expect("removing prompt text cannot exceed a resource limit");
    }

    fn move_to_position(&mut self, target: PromptPosition, selecting: bool) {
        self.assert_current_position(target);
        self.move_cursor(target.byte, selecting);
    }

    fn assert_current_position(&self, position: PromptPosition) {
        assert_eq!(
            position.revision, self.revision,
            "prompt position belongs to the current editor revision"
        );
        assert!(
            is_grapheme_boundary(&self.text, position.byte),
            "prompt layout produces grapheme-boundary positions"
        );
    }

    fn move_cursor(&mut self, target: usize, selecting: bool) {
        debug_assert!(is_grapheme_boundary(&self.text, target));
        if selecting {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = target;
        self.clear_collapsed_selection();
    }

    fn clear_collapsed_selection(&mut self) {
        if self.selection_anchor == Some(self.cursor) {
            self.selection_anchor = None;
        }
    }

    fn replace_range(
        &mut self,
        range: Range<usize>,
        inserted: String,
    ) -> Result<(), PromptCapacityError> {
        debug_assert!(range.start <= range.end);
        debug_assert!(self.text.is_char_boundary(range.start));
        debug_assert!(self.text.is_char_boundary(range.end));
        let removed = self.text[range.clone()].to_string();
        let resulting_bytes = self
            .text
            .len()
            .saturating_sub(removed.len())
            .saturating_add(inserted.len());
        if resulting_bytes > MAX_PROMPT_BYTES {
            return Err(PromptCapacityError {
                actual_bytes: resulting_bytes,
                maximum_bytes: MAX_PROMPT_BYTES,
            });
        }

        let before = self.editor_state();
        let start = range.start;
        self.text.replace_range(range, &inserted);
        let intended_cursor = start.saturating_add(inserted.len());
        self.cursor = grapheme_boundary_at_or_after(&self.text, intended_cursor);
        self.selection_anchor = None;
        self.vertical_goal_cell = None;
        let after = self.editor_state();
        self.history.record(EditRecord {
            start,
            removed,
            inserted,
            before,
            after,
        });
        self.bump_revision();
        self.debug_assert_invariants();
        Ok(())
    }

    fn editor_state(&self) -> EditorState {
        EditorState {
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
        }
    }

    fn restore_editor_state(&mut self, state: EditorState) {
        self.cursor = state.cursor;
        self.selection_anchor = state.selection_anchor;
        self.vertical_goal_cell = None;
    }

    fn debug_assert_invariants(&self) {
        debug_assert!(self.text.len() <= MAX_PROMPT_BYTES);
        debug_assert!(is_grapheme_boundary(&self.text, self.cursor));
        debug_assert!(
            self.selection_anchor
                .is_none_or(|anchor| is_grapheme_boundary(&self.text, anchor))
        );
    }

    fn ensure_layout(&mut self, width: u16) {
        if self
            .layout
            .as_ref()
            .is_some_and(|layout| layout.matches(width, self.revision))
        {
            return;
        }
        self.layout = Some(PromptLayout::new(&self.text, width, self.revision));
    }

    fn bump_revision(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("prompt revision space is not exhausted");
        self.layout = None;
    }
}

fn is_grapheme_boundary(text: &str, cursor: usize) -> bool {
    cursor == text.len()
        || text
            .grapheme_indices(true)
            .any(|(boundary, _)| boundary == cursor)
}

fn grapheme_boundary_at_or_after(text: &str, byte: usize) -> usize {
    debug_assert!(byte <= text.len());
    debug_assert!(text.is_char_boundary(byte));
    text.grapheme_indices(true)
        .map(|(boundary, _)| boundary)
        .find(|boundary| *boundary >= byte)
        .unwrap_or(text.len())
}

fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    debug_assert!(is_grapheme_boundary(text, cursor));
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    debug_assert!(is_grapheme_boundary(text, cursor));
    text[cursor..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(offset, _)| cursor + offset)
        .unwrap_or(text.len())
}

fn previous_word_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor;
    while cursor > 0 {
        let previous = previous_grapheme_boundary(text, cursor);
        if !text[previous..cursor].chars().all(char::is_whitespace) {
            break;
        }
        cursor = previous;
    }
    while cursor > 0 {
        let previous = previous_grapheme_boundary(text, cursor);
        if text[previous..cursor].chars().all(char::is_whitespace) {
            break;
        }
        cursor = previous;
    }
    cursor
}

fn next_word_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor;
    while cursor < text.len() {
        let next = next_grapheme_boundary(text, cursor);
        if text[cursor..next].chars().all(char::is_whitespace) {
            break;
        }
        cursor = next;
    }
    while cursor < text.len() {
        let next = next_grapheme_boundary(text, cursor);
        if !text[cursor..next].chars().all(char::is_whitespace) {
            break;
        }
        cursor = next;
    }
    cursor
}

fn logical_line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

fn logical_line_end(text: &str, cursor: usize) -> usize {
    let line_feed = text[cursor..]
        .find('\n')
        .map_or(text.len(), |offset| cursor + offset);
    if text[..line_feed].ends_with('\r') {
        line_feed - 1
    } else {
        line_feed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounded(text: &str) -> InputFragment<BoundedInput> {
        InputFragment::<RawInput>::new(text)
            .bound()
            .expect("test input is bounded")
    }

    #[test]
    fn editor_never_places_cursor_inside_grapheme() {
        let text = "a👨‍👩‍👧‍👦e\u{301}".to_string();
        let mut editor = PromptEditor::import(text.clone(), text.len()).unwrap();
        editor.move_horizontal(std::cmp::Ordering::Less, HorizontalUnit::Grapheme, false);
        assert_eq!(&text[editor.cursor()..], "e\u{301}");
        editor.delete_backward();
        assert_eq!(editor.text(), "ae\u{301}");
    }

    #[test]
    fn logical_line_end_stops_at_the_crlf_grapheme_boundary() {
        let mut editor = PromptEditor::import("a\r\nb".to_string(), 0).unwrap();

        editor.move_to_line_boundary(true, false);
        assert_eq!(editor.cursor(), 1);
        editor.move_horizontal(std::cmp::Ordering::Greater, HorizontalUnit::Grapheme, false);
        assert_eq!(editor.cursor(), 3);
        editor.move_to_line_boundary(true, false);
        assert_eq!(editor.cursor(), 4);
    }

    #[test]
    fn submission_clears_only_after_commit() {
        let mut editor = PromptEditor::import("send me".to_string(), 7).unwrap();
        let submission = editor.prepare_submission().unwrap();
        assert_eq!(editor.text(), "send me");
        assert_eq!(submission.text(), "send me");
        editor.commit_submission(submission.token());
        assert!(editor.is_empty());
    }

    #[test]
    fn one_replacement_is_one_undo_transaction() {
        let mut editor = PromptEditor::import("alpha beta".to_string(), 6).unwrap();
        editor.move_horizontal(std::cmp::Ordering::Greater, HorizontalUnit::Word, true);
        editor.insert(bounded("X")).unwrap();
        assert_eq!(editor.text(), "alpha X");
        editor.undo();
        assert_eq!(editor.text(), "alpha beta");
    }
    #[test]
    fn replacement_undo_and_redo_store_reversible_operations() {
        let mut editor = PromptEditor::import("before".to_string(), 0).unwrap();
        editor.move_to_line_boundary(true, true);
        editor.insert(bounded("after")).unwrap();

        editor.undo();
        assert_eq!(editor.text(), "before");
        assert_eq!(editor.selection(), Some(0.."before".len()));

        editor.redo();
        assert_eq!(editor.text(), "after");
        assert_eq!(editor.cursor(), "after".len());
        assert_eq!(editor.selection(), None);
    }

    #[test]
    fn edit_seams_resolve_to_a_valid_grapheme_boundary() {
        let mut editor = PromptEditor::import("a".to_string(), 1).unwrap();
        editor.insert(bounded("\u{200d}")).unwrap();

        assert_eq!(editor.text(), "a\u{200d}");
        assert!(is_grapheme_boundary(editor.text(), editor.cursor()));
    }

    #[test]
    fn oversized_prompt_is_rejected_without_mutation() {
        let mut editor = PromptEditor::import("kept".to_string(), 4).unwrap();
        let oversized = InputFragment::<RawInput>::new("x".repeat(MAX_PROMPT_BYTES + 1)).bound();
        assert_eq!(oversized.unwrap_err().actual_bytes(), MAX_PROMPT_BYTES + 1);
        assert_eq!(editor.text(), "kept");

        let remaining = MAX_PROMPT_BYTES - editor.text().len();
        editor
            .insert(bounded(&"x".repeat(remaining)))
            .expect("prompt reaches its exact byte bound");
        let error = editor.insert(bounded("y")).unwrap_err();
        assert_eq!(error.actual_bytes(), MAX_PROMPT_BYTES + 1);
        assert_eq!(editor.text().len(), MAX_PROMPT_BYTES);
    }
}
