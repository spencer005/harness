//! Transcript document storage with stable and persisted identities.

use std::collections::{HashMap, HashSet};


use crate::domain::{
    ExternalText, MessageRole, PersistedTranscriptEntry, TranscriptPayload, TranscriptSnapshotEntry,
};

/// Stable in-memory transcript entry identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TranscriptEntryId(u64);

impl TranscriptEntryId {
    /// Returns the numeric identifier.
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

/// One transcript record with identity, payload, and revision in one allocation.
#[derive(Debug, Clone)]
pub(crate) struct TranscriptEntry {
    id: TranscriptEntryId,
    source_sequence: Option<u64>,
    revision: u64,
    payload: TranscriptPayload,
}

impl TranscriptEntry {
    /// Returns stable in-memory identity.
    pub(crate) fn id(&self) -> TranscriptEntryId {
        self.id
    }

    /// Returns content revision.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns semantic payload.
    pub(crate) fn payload(&self) -> &TranscriptPayload {
        &self.payload
    }
}

/// Ordered transcript document with constant-time identity lookup.
#[derive(Debug)]
pub(super) struct TranscriptDocument {
    entries: Vec<TranscriptEntry>,
    id_index: HashMap<TranscriptEntryId, usize>,
    sequence_index: HashMap<u64, TranscriptEntryId>,
    next_id: u64,
    revision: u64,
}

impl TranscriptDocument {
    pub(crate) fn from_snapshot(
        entries: Vec<TranscriptSnapshotEntry>,
    ) -> Result<Self, crate::transcript::TranscriptError> {
        let mut document = Self {
            entries: Vec::with_capacity(entries.len()),
            id_index: HashMap::with_capacity(entries.len()),
            sequence_index: HashMap::new(),
            next_id: 1,
            revision: 0,
        };
        for entry in entries {
            if let Some(sequence) = entry.sequence
                && document.sequence_index.contains_key(&sequence)
            {
                return Err(crate::transcript::TranscriptError::ConflictingSequence(sequence));
            }
            document.insert_tail(entry.sequence, entry.payload);
        }
        Ok(document)
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = &TranscriptEntry> {
        self.entries.iter()
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn index_of(&self, id: TranscriptEntryId) -> Option<usize> {
        self.id_index.get(&id).copied()
    }

    pub(super) fn entry(&self, id: TranscriptEntryId) -> Option<&TranscriptEntry> {
        self.index_of(id).and_then(|index| self.entries.get(index))
    }

    pub(super) fn entry_at(&self, index: usize) -> Option<&TranscriptEntry> {
        self.entries.get(index)
    }

    pub(super) fn push(&mut self, payload: TranscriptPayload) -> TranscriptEntryId {
        self.insert_tail(None, payload)
    }

    pub(crate) fn insert_snapshot(
        &mut self,
        entry: TranscriptSnapshotEntry,
    ) -> Result<TranscriptEntryId, crate::transcript::TranscriptError> {
        if let Some(sequence) = entry.sequence {
            if self.sequence_index.contains_key(&sequence) {
                return Err(crate::transcript::TranscriptError::ConflictingSequence(sequence));
            }
        }
        Ok(self.insert_tail(entry.sequence, entry.payload))
    }

    pub(crate) fn attach_sequence(&mut self, id: TranscriptEntryId, sequence: u64) -> Result<(), crate::transcript::TranscriptError> {
        if self.sequence_index.contains_key(&sequence) {
            return Err(crate::transcript::TranscriptError::ConflictingSequence(sequence));
        }
        let Some(index) = self.index_of(id) else {
            return Ok(());
        };
        let entry = &mut self.entries[index];
        entry.source_sequence = Some(sequence);
        self.sequence_index.insert(sequence, id);
        self.bump_revision();
        Ok(())
    }

    pub(super) fn append_assistant_text(
        &mut self,
        id: TranscriptEntryId,
        delta: &ExternalText,
    ) {
        let Some(index) = self.index_of(id) else { return; };
        let entry = &mut self.entries[index];
        let TranscriptPayload::Message {
            role: MessageRole::Assistant,
            text,
        } = &mut entry.payload
        else {
            return;
        };
        text.append(delta);
        entry.revision = entry
            .revision
            .checked_add(1)
            .expect("transcript entry revision space is not exhausted");
        self.bump_revision();
    }

    pub(super) fn append_thinking_text(
        &mut self,
        id: TranscriptEntryId,
        delta: &ExternalText,
    ) {
        let Some(index) = self.index_of(id) else { return; };
        let entry = &mut self.entries[index];
        let TranscriptPayload::Thinking(text) = &mut entry.payload else {
            return;
        };
        text.append(delta);
        entry.revision = entry
            .revision
            .checked_add(1)
            .expect("transcript entry revision space is not exhausted");
        self.bump_revision();
    }

    pub(crate) fn novel_page_entries(
        &self,
        entries: Vec<PersistedTranscriptEntry>,
    ) -> Result<Vec<PersistedTranscriptEntry>, crate::transcript::TranscriptError> {
        let mut page_sequences = std::collections::HashSet::with_capacity(entries.len());
        let mut novel = Vec::with_capacity(entries.len());

        for entry in entries {
            if !page_sequences.insert(entry.sequence) {
                continue;
            }
            if self.sequence_index.contains_key(&entry.sequence) {
                return Err(crate::transcript::TranscriptError::ConflictingSequence(entry.sequence));
            }
            novel.push(entry);
        }

        novel.sort_by_key(|entry| entry.sequence);
        Ok(novel)
    }

    pub(super) fn prepend_page(&mut self, entries: Vec<PersistedTranscriptEntry>) {
        if entries.is_empty() {
            return;
        }
        let mut inserted = Vec::with_capacity(entries.len());
        for entry in entries {
            let id = self.allocate_id();
            self.sequence_index.insert(entry.sequence, id);
            inserted.push(TranscriptEntry {
                id,
                source_sequence: Some(entry.sequence),
                revision: 0,
                payload: entry.payload,
            });
        }
        self.entries.splice(0..0, inserted);
        self.rebuild_id_index();
        self.bump_revision();
    }

    fn insert_tail(
        &mut self,
        source_sequence: Option<u64>,
        payload: TranscriptPayload,
    ) -> TranscriptEntryId {
        let id = self.allocate_id();
        let index = self.entries.len();
        self.entries.push(TranscriptEntry {
            id,
            source_sequence,
            revision: 0,
            payload,
        });
        self.id_index.insert(id, index);
        if let Some(sequence) = source_sequence {
            self.sequence_index.insert(sequence, id);
        }
        self.bump_revision();
        id
    }

    fn allocate_id(&mut self) -> TranscriptEntryId {
        let id = TranscriptEntryId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("transcript entry identifier space is not exhausted");
        id
    }

    pub(super) fn into_snapshot_entries(self) -> Vec<TranscriptSnapshotEntry> {
        self.entries
            .into_iter()
            .map(|entry| TranscriptSnapshotEntry {
                sequence: entry.source_sequence,
                payload: entry.payload,
            })
            .collect()
    }

    fn rebuild_id_index(&mut self) {
        self.id_index.clear();
        self.id_index.extend(
            self.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.id, index)),
        );
    }

    fn bump_revision(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("transcript document revision space is not exhausted");
    }
}
