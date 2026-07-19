//! Operation-based bounded prompt edit history.

use std::collections::VecDeque;

/// Cursor and selection state restored with an edit operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EditorState {
    pub(super) cursor: usize,
    pub(super) selection_anchor: Option<usize>,
}

/// One reversible range replacement.
#[derive(Debug, Clone)]
pub(super) struct EditRecord {
    pub(super) start: usize,
    pub(super) removed: String,
    pub(super) inserted: String,
    pub(super) before: EditorState,
    pub(super) after: EditorState,
}

impl EditRecord {
    fn retained_bytes(&self) -> usize {
        self.removed
            .len()
            .saturating_add(self.inserted.len())
            .saturating_add(std::mem::size_of::<Self>())
    }
}

/// Bounded undo and redo stacks storing edit deltas rather than draft copies.
#[derive(Debug, Clone, Default)]
pub(super) struct EditHistory {
    undo: VecDeque<EditRecord>,
    redo: Vec<EditRecord>,
    retained_bytes: usize,
}

impl EditHistory {
    const MAX_RECORDS: usize = 256;
    const MAX_BYTES: usize = super::MAX_PROMPT_BYTES * 2 + 4 * 1024;

    pub(super) fn record(&mut self, record: EditRecord) {
        self.clear_redo();
        self.retained_bytes = self.retained_bytes.saturating_add(record.retained_bytes());
        self.undo.push_back(record);
        self.trim_oldest_undo();
    }

    pub(super) fn pop_undo(&mut self) -> Option<EditRecord> {
        let record = self.undo.pop_back()?;
        self.retained_bytes = self.retained_bytes.saturating_sub(record.retained_bytes());
        Some(record)
    }

    pub(super) fn push_redo(&mut self, record: EditRecord) {
        self.retained_bytes = self.retained_bytes.saturating_add(record.retained_bytes());
        self.redo.push(record);
        debug_assert!(self.within_bounds());
    }

    pub(super) fn pop_redo(&mut self) -> Option<EditRecord> {
        let record = self.redo.pop()?;
        self.retained_bytes = self.retained_bytes.saturating_sub(record.retained_bytes());
        Some(record)
    }

    pub(super) fn push_undo_from_redo(&mut self, record: EditRecord) {
        self.retained_bytes = self.retained_bytes.saturating_add(record.retained_bytes());
        self.undo.push_back(record);
        debug_assert!(self.within_bounds());
    }

    pub(super) fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.retained_bytes = 0;
    }

    fn clear_redo(&mut self) {
        self.retained_bytes = self.redo.iter().fold(self.retained_bytes, |bytes, record| {
            bytes.saturating_sub(record.retained_bytes())
        });
        self.redo.clear();
    }

    fn trim_oldest_undo(&mut self) {
        while self.undo.len().saturating_add(self.redo.len()) > Self::MAX_RECORDS
            || self.retained_bytes > Self::MAX_BYTES
        {
            let removed = self
                .undo
                .pop_front()
                .expect("a newly recorded undo operation exceeds a history bound");
            self.retained_bytes = self.retained_bytes.saturating_sub(removed.retained_bytes());
        }
        debug_assert!(self.within_bounds());
    }

    fn within_bounds(&self) -> bool {
        self.undo.len().saturating_add(self.redo.len()) <= Self::MAX_RECORDS
            && self.retained_bytes <= Self::MAX_BYTES
    }
}
