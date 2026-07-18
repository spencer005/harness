use std::ops::Range;

use harness_core::UiSnapshot;

/// Edits the prompt input stored in `UiSnapshot`.
///
/// `InputEditor` owns edit history and cursor movement intent. The active input
/// text and cursor position remain in `UiSnapshot`, so each mutating method
/// requires an explicit mutable snapshot argument.
#[derive(Debug, Clone, Default)]
pub struct InputEditor {
    undo_history: Vec<InputEditState>,
    redo_history: Vec<InputEditState>,
    goal_column: Option<usize>,
    selection_anchor: Option<usize>,
    mouse_selecting: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputEditState {
    input: String,
    input_cursor: usize,
}

/// Vertical cursor movement direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalDirection {
    /// Move to the previous visual or logical line.
    Up,
    /// Move to the next visual or logical line.
    Down,
}

impl InputEditor {
    /// Insert one character at the current input cursor.
    pub fn insert_char(&mut self, snapshot: &mut UiSnapshot, ch: char) {
        self.goal_column = None;
        clamp_input_cursor(snapshot);
        self.push_undo(snapshot);
        self.delete_selection_after_undo(snapshot);
        snapshot.input.insert(snapshot.input_cursor, ch);
        snapshot.input_cursor += ch.len_utf8();
    }

    /// Insert bracketed paste content after normalizing line endings.
    pub fn insert_paste_text(&mut self, snapshot: &mut UiSnapshot, text: &str) {
        if text.is_empty() {
            return;
        }
        self.goal_column = None;
        clamp_input_cursor(snapshot);
        self.push_undo(snapshot);
        self.delete_selection_after_undo(snapshot);
        let text = normalize_paste_text(text);
        snapshot.input.insert_str(snapshot.input_cursor, &text);
        snapshot.input_cursor += text.len();
    }

    /// Delete one character before the current input cursor.
    pub fn delete_char_before_cursor(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        clamp_input_cursor(snapshot);
        if self.selection_range(snapshot).is_some() {
            self.push_undo(snapshot);
            self.delete_selection_after_undo(snapshot);
            return;
        }
        let start = previous_char_boundary(&snapshot.input, snapshot.input_cursor);
        if start == snapshot.input_cursor {
            return;
        }
        self.push_undo(snapshot);
        snapshot.input.drain(start..snapshot.input_cursor);
        snapshot.input_cursor = start;
    }

    /// Delete one character at the current input cursor.
    pub fn delete_char_at_cursor(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        clamp_input_cursor(snapshot);
        if self.selection_range(snapshot).is_some() {
            self.push_undo(snapshot);
            self.delete_selection_after_undo(snapshot);
            return;
        }
        let end = next_char_boundary(&snapshot.input, snapshot.input_cursor);
        if end == snapshot.input_cursor {
            return;
        }
        self.push_undo(snapshot);
        snapshot.input.drain(snapshot.input_cursor..end);
    }

    /// Delete the word before the current input cursor.
    pub fn delete_word_before_cursor(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        clamp_input_cursor(snapshot);
        if self.selection_range(snapshot).is_some() {
            self.push_undo(snapshot);
            self.delete_selection_after_undo(snapshot);
            return;
        }
        let start = previous_word_start(&snapshot.input, snapshot.input_cursor);
        if start == snapshot.input_cursor {
            return;
        }
        self.push_undo(snapshot);
        snapshot.input.drain(start..snapshot.input_cursor);
        snapshot.input_cursor = start;
    }

    /// Clear the current input.
    pub fn clear(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        self.selection_anchor = None;
        self.mouse_selecting = false;
        if snapshot.input.is_empty() {
            snapshot.input_cursor = 0;
            return;
        }
        self.push_undo(snapshot);
        snapshot.input.clear();
        snapshot.input_cursor = 0;
    }

    /// Take trimmed input text for steering and clear edit history.
    pub fn take_trimmed_text(&mut self, snapshot: &mut UiSnapshot) -> String {
        self.goal_column = None;
        self.selection_anchor = None;
        self.mouse_selecting = false;
        let input = snapshot.input.trim().to_string();
        snapshot.input.clear();
        snapshot.input_cursor = 0;
        self.clear_history();
        input
    }

    /// Submit current input when it contains non-whitespace text.
    ///
    /// The snapshot input is cleared even when the submitted value is empty
    /// after trimming, matching terminal prompt behavior.
    pub fn submit(&mut self, snapshot: &mut UiSnapshot) -> Option<String> {
        self.goal_column = None;
        self.selection_anchor = None;
        self.mouse_selecting = false;
        let input = snapshot.input.clone();
        snapshot.input.clear();
        snapshot.input_cursor = 0;
        self.clear_history();
        (!input.trim().is_empty()).then_some(input)
    }

    /// Move the cursor to the start of the current logical line.
    pub fn move_to_line_start(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = line_start(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, false);
    }

    /// Move the cursor to the end of the current logical line.
    pub fn move_to_line_end(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = line_end(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, false);
    }

    /// Move the cursor to the previous character boundary.
    pub fn move_to_previous_char(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = previous_char_boundary(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, false);
    }

    /// Move the cursor to the next character boundary.
    pub fn move_to_next_char(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = next_char_boundary(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, false);
    }

    /// Move the cursor to the previous word boundary.
    pub fn move_to_previous_word(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = previous_word_start(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, false);
    }

    /// Move the cursor to the next word boundary.
    pub fn move_to_next_word(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = next_word_start(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, false);
    }

    /// Extend selection to the previous character boundary.
    pub fn select_to_previous_char(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = previous_char_boundary(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, true);
    }

    /// Extend selection to the next character boundary.
    pub fn select_to_next_char(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = next_char_boundary(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, true);
    }

    /// Extend selection to the previous word boundary.
    pub fn select_to_previous_word(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = previous_word_start(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, true);
    }

    /// Extend selection to the next word boundary.
    pub fn select_to_next_word(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        let target = next_word_start(&snapshot.input, snapshot.input_cursor);
        self.move_cursor_to(snapshot, target, true);
    }

    /// Move the cursor vertically while preserving the desired character column.
    pub fn move_vertically(&mut self, snapshot: &mut UiSnapshot, direction: VerticalDirection) {
        self.move_vertically_with_selection(snapshot, direction, false);
    }

    /// Extend selection vertically while preserving the desired character column.
    pub fn select_vertically(&mut self, snapshot: &mut UiSnapshot, direction: VerticalDirection) {
        self.move_vertically_with_selection(snapshot, direction, true);
    }

    /// Start a mouse selection at the provided cursor byte index.
    pub fn begin_mouse_selection(&mut self, snapshot: &mut UiSnapshot, cursor: usize) {
        self.goal_column = None;
        let cursor = floor_char_boundary(&snapshot.input, cursor);
        snapshot.input_cursor = cursor;
        self.selection_anchor = Some(cursor);
        self.mouse_selecting = true;
    }

    /// Extend the active mouse selection to the provided cursor byte index.
    pub fn drag_mouse_selection(&mut self, snapshot: &mut UiSnapshot, cursor: usize) {
        self.goal_column = None;
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(snapshot.input_cursor);
        }
        snapshot.input_cursor = floor_char_boundary(&snapshot.input, cursor);
        self.clear_collapsed_selection(snapshot);
    }

    /// Complete mouse selection and collapse empty ranges.
    pub fn finish_mouse_selection(&mut self, snapshot: &mut UiSnapshot, cursor: usize) {
        self.drag_mouse_selection(snapshot, cursor);
        self.mouse_selecting = false;
    }

    /// Clear any active input selection.
    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.mouse_selecting = false;
    }

    /// Return true while a mouse-originated selection gesture is active.
    pub fn mouse_selection_active(&self) -> bool {
        self.mouse_selecting
    }

    /// Return the selected byte range in the current input, when non-empty.
    pub fn selection_range(&self, snapshot: &UiSnapshot) -> Option<Range<usize>> {
        let anchor = self.selection_anchor?;
        let anchor = floor_char_boundary(&snapshot.input, anchor);
        let cursor = floor_char_boundary(&snapshot.input, snapshot.input_cursor);
        if anchor == cursor {
            None
        } else if anchor < cursor {
            Some(anchor..cursor)
        } else {
            Some(cursor..anchor)
        }
    }

    fn move_vertically_with_selection(
        &mut self,
        snapshot: &mut UiSnapshot,
        direction: VerticalDirection,
        selecting: bool,
    ) {
        clamp_input_cursor(snapshot);
        let cursor = snapshot.input_cursor;
        let current_start = line_start(&snapshot.input, cursor);
        let current_end = line_end(&snapshot.input, cursor);
        let current_col = snapshot.input[current_start..cursor].chars().count();
        let target_col = self.goal_column.unwrap_or(current_col);
        let Some((target_start, target_end)) =
            target_line_range(&snapshot.input, current_start, current_end, direction)
        else {
            return;
        };
        let target =
            byte_index_for_char_column(&snapshot.input, target_start, target_end, target_col);
        self.move_cursor_to(snapshot, target, selecting);
        self.goal_column = Some(target_col);
    }

    /// Restore the previous input edit state when history is available.
    pub fn undo(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        self.selection_anchor = None;
        self.mouse_selecting = false;
        let Some(previous) = self.undo_history.pop() else {
            return;
        };
        self.redo_history.push(InputEditState {
            input: snapshot.input.clone(),
            input_cursor: snapshot.input_cursor,
        });
        snapshot.input = previous.input;
        snapshot.input_cursor = previous.input_cursor;
        clamp_input_cursor(snapshot);
    }

    /// Reapply the next input edit state when history is available.
    pub fn redo(&mut self, snapshot: &mut UiSnapshot) {
        self.goal_column = None;
        self.selection_anchor = None;
        self.mouse_selecting = false;
        let Some(next) = self.redo_history.pop() else {
            return;
        };
        self.undo_history.push(InputEditState {
            input: snapshot.input.clone(),
            input_cursor: snapshot.input_cursor,
        });
        snapshot.input = next.input;
        snapshot.input_cursor = next.input_cursor;
        clamp_input_cursor(snapshot);
    }

    fn push_undo(&mut self, snapshot: &UiSnapshot) {
        self.undo_history.push(InputEditState {
            input: snapshot.input.clone(),
            input_cursor: snapshot.input_cursor,
        });
        self.redo_history.clear();
    }

    fn clear_history(&mut self) {
        self.undo_history.clear();
        self.redo_history.clear();
    }

    fn move_cursor_to(&mut self, snapshot: &mut UiSnapshot, cursor: usize, selecting: bool) {
        clamp_input_cursor(snapshot);
        let cursor = floor_char_boundary(&snapshot.input, cursor);
        if selecting {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(snapshot.input_cursor);
            }
            snapshot.input_cursor = cursor;
            self.clear_collapsed_selection(snapshot);
        } else {
            snapshot.input_cursor = cursor;
            self.selection_anchor = None;
            self.mouse_selecting = false;
        }
    }

    fn delete_selection_after_undo(&mut self, snapshot: &mut UiSnapshot) -> bool {
        let Some(range) = self.selection_range(snapshot) else {
            return false;
        };
        snapshot.input.drain(range.clone());
        snapshot.input_cursor = range.start;
        self.selection_anchor = None;
        self.mouse_selecting = false;
        true
    }

    fn clear_collapsed_selection(&mut self, snapshot: &UiSnapshot) {
        if self.selection_range(snapshot).is_none() {
            self.selection_anchor = None;
            self.mouse_selecting = false;
        }
    }
}

/// Clamp the snapshot input cursor to a valid UTF-8 character boundary.
pub fn clamp_input_cursor(snapshot: &mut UiSnapshot) {
    snapshot.input_cursor = floor_char_boundary(&snapshot.input, snapshot.input_cursor);
}

/// Return the nearest UTF-8 character boundary at or before `cursor`.
pub fn floor_char_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// Return the UTF-8 character boundary before `cursor`.
pub fn previous_char_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    if cursor == 0 {
        return 0;
    }
    text[..cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// Return the UTF-8 character boundary after `cursor`.
pub fn next_char_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    if cursor == text.len() {
        return cursor;
    }
    text[cursor..]
        .char_indices()
        .nth(1)
        .map(|(index, _)| cursor + index)
        .unwrap_or(text.len())
}

/// Return the byte index at the start of the cursor's logical line.
pub fn line_start(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[..cursor]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

/// Return the byte index at the end of the cursor's logical line.
pub fn line_end(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[cursor..]
        .find('\n')
        .map(|index| cursor + index)
        .unwrap_or(text.len())
}

/// Return the start byte index of the word before `cursor`.
pub fn previous_word_start(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 {
        let previous = previous_char_boundary(text, cursor);
        let ch = text[previous..cursor]
            .chars()
            .next()
            .expect("previous char");
        if !ch.is_whitespace() {
            break;
        }
        cursor = previous;
    }
    while cursor > 0 {
        let previous = previous_char_boundary(text, cursor);
        let ch = text[previous..cursor]
            .chars()
            .next()
            .expect("previous char");
        if ch.is_whitespace() {
            break;
        }
        cursor = previous;
    }
    cursor
}

/// Return the start byte index of the next word after `cursor`.
pub fn next_word_start(text: &str, cursor: usize) -> usize {
    let mut cursor = floor_char_boundary(text, cursor.min(text.len()));
    while cursor < text.len() {
        let next = next_char_boundary(text, cursor);
        let ch = text[cursor..next].chars().next().expect("next char");
        if ch.is_whitespace() {
            break;
        }
        cursor = next;
    }
    while cursor < text.len() {
        let next = next_char_boundary(text, cursor);
        let ch = text[cursor..next].chars().next().expect("next char");
        if !ch.is_whitespace() {
            break;
        }
        cursor = next;
    }
    cursor
}

/// Return the logical line range reached by vertical cursor movement.
pub fn target_line_range(
    text: &str,
    current_start: usize,
    current_end: usize,
    direction: VerticalDirection,
) -> Option<(usize, usize)> {
    match direction {
        VerticalDirection::Up => {
            if current_start == 0 {
                return None;
            }
            let previous_end = current_start - 1;
            let previous_start = line_start(text, previous_end);
            Some((previous_start, previous_end))
        }
        VerticalDirection::Down => {
            if current_end == text.len() {
                return None;
            }
            let next_start = current_end + 1;
            Some((next_start, line_end(text, next_start)))
        }
    }
}

/// Return the byte index for a character column inside a byte range.
pub fn byte_index_for_char_column(
    text: &str,
    start: usize,
    end: usize,
    target_col: usize,
) -> usize {
    text[start..end]
        .char_indices()
        .nth(target_col)
        .map(|(offset, _)| start + offset)
        .unwrap_or(end)
}

/// Normalize pasted text to use line-feed newlines.
pub fn normalize_paste_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_mutates_only_the_provided_snapshot_input() {
        let mut snapshot = UiSnapshot::default();
        let mut editor = InputEditor::default();

        editor.insert_char(&mut snapshot, 'h');
        editor.insert_char(&mut snapshot, 'i');
        editor.move_to_previous_char(&mut snapshot);
        editor.insert_char(&mut snapshot, '!');

        assert_eq!(snapshot.input, "h!i");
        assert_eq!(snapshot.input_cursor, 2);
    }

    #[test]
    fn editor_undo_and_redo_restore_snapshot_input_state() {
        let mut snapshot = UiSnapshot::default();
        let mut editor = InputEditor::default();

        editor.insert_char(&mut snapshot, 'a');
        editor.insert_char(&mut snapshot, 'b');
        editor.undo(&mut snapshot);
        assert_eq!(snapshot.input, "a");
        assert_eq!(snapshot.input_cursor, 1);

        editor.redo(&mut snapshot);
        assert_eq!(snapshot.input, "ab");
        assert_eq!(snapshot.input_cursor, 2);
    }

    #[test]
    fn editor_word_navigation_skips_whitespace_between_words() {
        let mut snapshot = UiSnapshot {
            input: "alpha  beta gamma".to_string(),
            input_cursor: "alpha  beta gamma".len(),
            ..Default::default()
        };
        let mut editor = InputEditor::default();

        editor.move_to_previous_word(&mut snapshot);
        assert_eq!(snapshot.input_cursor, "alpha  beta ".len());

        editor.move_to_next_word(&mut snapshot);
        assert_eq!(snapshot.input_cursor, "alpha  beta gamma".len());
    }

    #[test]
    fn editor_selection_replaces_text_with_single_undo_state() {
        let mut snapshot = UiSnapshot {
            input: "alpha beta".to_string(),
            input_cursor: "alpha ".len(),
            ..Default::default()
        };
        let mut editor = InputEditor::default();

        editor.select_to_next_word(&mut snapshot);
        assert_eq!(
            editor.selection_range(&snapshot),
            Some("alpha ".len().."alpha beta".len())
        );
        editor.insert_char(&mut snapshot, 'X');
        assert_eq!(snapshot.input, "alpha X");
        assert_eq!(snapshot.input_cursor, "alpha X".len());

        editor.undo(&mut snapshot);
        assert_eq!(snapshot.input, "alpha beta");
        assert_eq!(snapshot.input_cursor, "alpha beta".len());
        assert_eq!(editor.selection_range(&snapshot), None);
    }
}
