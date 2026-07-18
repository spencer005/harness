/// Stable transcript entry identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TranscriptEntryId(u64);

impl TranscriptEntryId {
    /// Return the numeric identifier value.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Return the stable numeric entry identifier.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Stable transcript entry metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptEntryMeta {
    id: TranscriptEntryId,
    revision: u64,
}

impl TranscriptEntryMeta {
    /// Return this entry's stable identifier.
    pub fn id(self) -> TranscriptEntryId {
        self.id
    }

    /// Return this entry's content revision.
    pub fn revision(self) -> u64 {
        self.revision
    }
}

fn transcript_entry_byte_len(entry: &harness_core::UiTranscriptEntry) -> usize {
    use harness_core::{UiTranscriptEntry, sessions::SessionRecordKind};

    match entry {
        UiTranscriptEntry::Text(text) => text.len(),
        UiTranscriptEntry::SessionRecord(kind) => match kind {
            SessionRecordKind::UserMessage(message)
            | SessionRecordKind::DeveloperMessage(message)
            | SessionRecordKind::AssistantMessage(message) => message.text.len(),
            SessionRecordKind::FreeformToolCall(call) => {
                call.name.len() + call.call_id.len() + call.input.len()
            }
            SessionRecordKind::FreeformToolOutput(output) => {
                output.call_id.len()
                    + output.transcript_output().len()
                    + output
                        .display
                        .as_ref()
                        .map(tool_output_display_byte_len)
                        .unwrap_or(0)
            }
            SessionRecordKind::FunctionToolCall(call) => {
                call.name.len() + call.call_id.len() + call.arguments.len()
            }
            SessionRecordKind::FunctionToolOutput(output) => {
                output.call_id.len() + output.transcript_output().len()
            }
            SessionRecordKind::SessionClosed(_) => std::mem::size_of::<u64>(),
            SessionRecordKind::SessionMeta(_)
            | SessionRecordKind::TurnContext(_)
            | SessionRecordKind::FreeformToolInputDelta(_)
            | SessionRecordKind::CompactionCheckpoint(_)
            | SessionRecordKind::ProviderSessionBinding(_) => 0,
        },
    }
}

fn tool_output_display_byte_len(
    display: &harness_core::sessions::ToolOutputDisplayRecord,
) -> usize {
    match display {
        harness_core::sessions::ToolOutputDisplayRecord::InspectRead(reads) => reads
            .iter()
            .map(|read| {
                read.path.len()
                    + read.lines.iter().map(String::len).sum::<usize>()
                    + std::mem::size_of::<usize>() * 4
            })
            .sum(),
    }
}

/// Transcript store with stable entry IDs and entry revisions.
///
/// The store owns transcript entries and metadata in lockstep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptStore {
    entries: Vec<harness_core::UiTranscriptEntry>,
    metadata: Vec<TranscriptEntryMeta>,
    next_id: u64,
    revision: u64,
    total_bytes: usize,
}

impl TranscriptStore {
    /// Build a transcript store from existing transcript entries.
    pub fn from_entries(entries: Vec<harness_core::UiTranscriptEntry>) -> Self {
        let total_bytes = entries.iter().map(transcript_entry_byte_len).sum();
        let metadata = (0..entries.len())
            .map(|index| TranscriptEntryMeta {
                id: TranscriptEntryId((index as u64) + 1),
                revision: 0,
            })
            .collect::<Vec<_>>();
        let next_id = (metadata.len() as u64) + 1;
        Self {
            entries,
            metadata,
            next_id,
            revision: 0,
            total_bytes,
        }
    }

    /// Return transcript entries.
    pub fn entries(&self) -> &[harness_core::UiTranscriptEntry] {
        &self.entries
    }

    /// Return store-level revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Return total transcript bytes.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Return the number of transcript entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether the transcript is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return metadata for the entry at `index`.
    pub fn metadata_at(&self, index: usize) -> Option<TranscriptEntryMeta> {
        self.metadata.get(index).copied()
    }

    /// Return transcript entry metadata in display order.
    pub fn metadata(&self) -> &[TranscriptEntryMeta] {
        &self.metadata
    }

    /// Return the current index for a stable entry ID.
    pub fn index_of(&self, id: TranscriptEntryId) -> Option<usize> {
        self.metadata.iter().position(|entry| entry.id == id)
    }

    fn parse_error_line(line: &str) -> Option<(&str, u32)> {
        if !line.starts_with("error: ") {
            return None;
        }
        if line.ends_with(')') {
            if let Some(idx) = line.rfind(" (x") {
                let count_str = &line[idx + 3..line.len() - 1];
                if let Ok(count) = count_str.parse::<u32>() {
                    return Some((&line[..idx], count));
                }
            }
        }
        Some((line, 1))
    }

    /// Append one transcript entry and return its stable entry ID.
    pub fn push_entry(&mut self, entry: harness_core::UiTranscriptEntry) -> TranscriptEntryId {
        if let (
            Some(harness_core::UiTranscriptEntry::Text(last_line)),
            harness_core::UiTranscriptEntry::Text(new_line),
        ) = (self.entries.last(), &entry)
        {
            if last_line.starts_with("error: ") && new_line.starts_with("error: ") {
                if let Some((base_last, count)) = Self::parse_error_line(last_line) {
                    if let Some((base_new, _)) = Self::parse_error_line(new_line) {
                        if base_last == base_new {
                            let last_id = self.metadata.last().unwrap().id;
                            let updated_line = format!("{} (x{})", base_last, count + 1);
                            self.replace_entry(last_id, updated_line);
                            return last_id;
                        }
                    }
                }
            }
        }

        let id = self.allocate_id();
        self.total_bytes = self
            .total_bytes
            .saturating_add(transcript_entry_byte_len(&entry));
        self.entries.push(entry);
        self.metadata.push(TranscriptEntryMeta { id, revision: 0 });
        self.bump_revision();
        id
    }

    /// Append one transcript line and return its stable entry ID.
    pub fn push_line(&mut self, line: String) -> TranscriptEntryId {
        self.push_entry(harness_core::UiTranscriptEntry::Text(line))
    }

    /// Prepend transcript entries in their display order and return their IDs.
    pub fn prepend_entries(
        &mut self,
        entries: Vec<harness_core::UiTranscriptEntry>,
    ) -> Vec<TranscriptEntryId> {
        let mut ids = Vec::with_capacity(entries.len());
        let mut metadata = Vec::with_capacity(entries.len());
        let mut added_bytes = 0usize;
        for entry in &entries {
            let id = self.allocate_id();
            ids.push(id);
            metadata.push(TranscriptEntryMeta { id, revision: 0 });
            added_bytes = added_bytes.saturating_add(transcript_entry_byte_len(entry));
        }
        self.total_bytes = self.total_bytes.saturating_add(added_bytes);
        self.entries.splice(0..0, entries);
        self.metadata.splice(0..0, metadata);
        self.bump_revision();
        ids
    }

    /// Insert one transcript line after an existing entry and return its stable entry ID.
    pub fn insert_after(
        &mut self,
        id: TranscriptEntryId,
        line: String,
    ) -> Option<TranscriptEntryId> {
        let index = self.index_of(id)?;
        let inserted_id = self.allocate_id();
        let entry = harness_core::UiTranscriptEntry::Text(line);
        self.total_bytes = self
            .total_bytes
            .saturating_add(transcript_entry_byte_len(&entry));
        self.entries.insert(index.saturating_add(1), entry);
        self.metadata.insert(
            index.saturating_add(1),
            TranscriptEntryMeta {
                id: inserted_id,
                revision: 0,
            },
        );
        self.bump_revision();
        Some(inserted_id)
    }

    /// Append text to an existing text entry.
    pub fn append_to_entry_with(
        &mut self,
        id: TranscriptEntryId,
        text: &str,
        mut normalize: impl FnMut(&mut String),
    ) -> Option<()> {
        let index = self.index_of(id)?;
        let old_len = transcript_entry_byte_len(&self.entries[index]);
        let harness_core::UiTranscriptEntry::Text(line) = &mut self.entries[index] else {
            return None;
        };
        line.push_str(text);
        normalize(line);
        self.total_bytes = self
            .total_bytes
            .saturating_sub(old_len)
            .saturating_add(transcript_entry_byte_len(&self.entries[index]));
        self.metadata[index].revision = self.metadata[index].revision.saturating_add(1);
        self.bump_revision();
        Some(())
    }

    /// Replace one entry text and increment its revision.
    pub fn replace_entry(&mut self, id: TranscriptEntryId, line: String) -> Option<()> {
        let index = self.index_of(id)?;
        let old_len = transcript_entry_byte_len(&self.entries[index]);
        let entry = harness_core::UiTranscriptEntry::Text(line);
        self.total_bytes = self
            .total_bytes
            .saturating_sub(old_len)
            .saturating_add(transcript_entry_byte_len(&entry));
        self.entries[index] = entry;
        self.metadata[index].revision = self.metadata[index].revision.saturating_add(1);
        self.bump_revision();
        Some(())
    }

    /// Trim every text entry in place with the provided trimming function.
    pub fn trim_lines(&mut self, mut trim: impl FnMut(&mut String)) {
        let mut total_bytes = 0usize;
        for (entry, metadata) in self.entries.iter_mut().zip(&mut self.metadata) {
            let old = transcript_entry_byte_len(entry);
            if let harness_core::UiTranscriptEntry::Text(line) = entry {
                trim(line);
            }
            let new_len = transcript_entry_byte_len(entry);
            if new_len != old {
                metadata.revision = metadata.revision.saturating_add(1);
                self.revision = self.revision.saturating_add(1);
            }
            total_bytes = total_bytes.saturating_add(new_len);
        }
        self.total_bytes = total_bytes;
    }

    /// Drop the oldest transcript entry.
    pub fn drop_oldest(&mut self) -> Option<TranscriptEntryId> {
        if self.entries.is_empty() {
            return None;
        }
        let entry = self.entries.remove(0);
        let metadata = self.metadata.remove(0);
        self.total_bytes = self
            .total_bytes
            .saturating_sub(transcript_entry_byte_len(&entry));
        self.bump_revision();
        Some(metadata.id)
    }

    /// Drop the newest transcript entry.
    pub fn drop_newest(&mut self) -> Option<TranscriptEntryId> {
        let entry = self.entries.pop()?;
        let metadata = self
            .metadata
            .pop()
            .expect("metadata length matches transcript entries");
        self.total_bytes = self
            .total_bytes
            .saturating_sub(transcript_entry_byte_len(&entry));
        self.bump_revision();
        Some(metadata.id)
    }

    /// Drop all transcript entries after an existing entry.
    pub fn truncate_after(&mut self, id: TranscriptEntryId) -> Option<Vec<TranscriptEntryId>> {
        let index = self.index_of(id)?;
        let split_index = index.saturating_add(1);
        if split_index >= self.entries.len() {
            return Some(Vec::new());
        }
        let removed_entries = self.entries.split_off(split_index);
        let removed_metadata = self.metadata.split_off(split_index);
        for entry in removed_entries {
            self.total_bytes = self
                .total_bytes
                .saturating_sub(transcript_entry_byte_len(&entry));
        }
        self.bump_revision();
        Some(
            removed_metadata
                .into_iter()
                .map(|metadata| metadata.id)
                .collect(),
        )
    }

    /// Consume the store and return transcript entries.
    pub fn into_entries(self) -> Vec<harness_core::UiTranscriptEntry> {
        self.entries
    }

    fn allocate_id(&mut self) -> TranscriptEntryId {
        let id = TranscriptEntryId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Index mapping transcript entries to wrapped visual lines.
pub struct TranscriptLineIndex {
    entries: Vec<TranscriptIndexedEntry>,
    total_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptIndexedEntry {
    entry_index: usize,
    start_line: usize,
    body_lines: usize,
    display_lines: usize,
    has_separator: bool,
}

/// Location of one visual line in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptLineAddress {
    /// Entry index in the transcript store.
    pub entry_index: usize,
    /// Line offset within the entry, including the separator position.
    pub entry_line: usize,
    /// Absolute visual-line index in the transcript.
    pub visual_line: usize,
    /// Number of rendered body lines for the entry.
    pub body_lines: usize,
    /// Whether the entry has a separator after its body lines.
    pub has_separator: bool,
}

/// Entry-local line range visible in a transcript viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptVisibleRange {
    /// Entry index in the transcript store.
    pub entry_index: usize,
    /// First visible entry-local line.
    pub entry_line_start: usize,
    /// Number of visible lines for the entry.
    pub line_count: usize,
    /// Absolute visual-line index for the first visible line.
    pub visual_line_start: usize,
    /// Number of rendered body lines for the entry.
    pub body_lines: usize,
    /// Whether the entry has a separator after its body lines.
    pub has_separator: bool,
}

impl TranscriptLineIndex {
    /// Build a visual-line index from transcript entries and a body line counter.
    pub fn build(
        entries: &[harness_core::UiTranscriptEntry],
        mut body_line_count: impl FnMut(&harness_core::UiTranscriptEntry) -> usize,
    ) -> Self {
        Self::build_with_index(entries, |_, entry| body_line_count(entry))
    }

    /// Build a visual-line index with access to each transcript entry index.
    pub fn build_with_index(
        entries: &[harness_core::UiTranscriptEntry],
        body_line_count: impl FnMut(usize, &harness_core::UiTranscriptEntry) -> usize,
    ) -> Self {
        Self::build_with_index_and_separator(entries, body_line_count, |_, _| true)
    }

    /// Build a visual-line index with access to each transcript entry index and separator pair.
    pub fn build_with_index_and_separator(
        entries: &[harness_core::UiTranscriptEntry],
        mut body_line_count: impl FnMut(usize, &harness_core::UiTranscriptEntry) -> usize,
        mut has_separator_between: impl FnMut(usize, usize) -> bool,
    ) -> Self {
        let body_line_counts = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| body_line_count(index, entry))
            .collect::<Vec<_>>();
        let mut next_visible_entries = vec![None; body_line_counts.len()];
        let mut next_visible_entry = None;
        for (entry_index, body_lines) in body_line_counts.iter().enumerate().rev() {
            next_visible_entries[entry_index] = next_visible_entry;
            if *body_lines > 0 {
                next_visible_entry = Some(entry_index);
            }
        }
        let mut indexed_entries = Vec::new();
        let mut total_lines = 0usize;

        for (entry_index, body_lines) in body_line_counts.into_iter().enumerate() {
            if body_lines == 0 {
                continue;
            }
            let has_separator = next_visible_entries[entry_index]
                .is_some_and(|next| has_separator_between(entry_index, next));
            let display_lines = body_lines + usize::from(has_separator);
            indexed_entries.push(TranscriptIndexedEntry {
                entry_index,
                start_line: total_lines,
                body_lines,
                display_lines,
                has_separator,
            });
            total_lines = total_lines.saturating_add(display_lines);
        }

        Self {
            entries: indexed_entries,
            total_lines,
        }
    }

    /// Return the total visual line count represented by the index.
    pub fn total_lines(&self) -> usize {
        self.total_lines
    }

    /// Return whether an entry has a separator after its body lines.
    pub fn entry_has_separator(&self, entry_index: usize) -> bool {
        self.entries
            .iter()
            .find(|entry| entry.entry_index == entry_index)
            .is_some_and(|entry| entry.has_separator)
    }

    /// Return the indexed address for one visual transcript line.
    pub fn line_address(&self, visual_line: usize) -> Option<TranscriptLineAddress> {
        let entry = self.entry_for_visual_line(visual_line)?;
        Some(TranscriptLineAddress {
            entry_index: entry.entry_index,
            entry_line: visual_line.saturating_sub(entry.start_line),
            visual_line,
            body_lines: entry.body_lines,
            has_separator: entry.has_separator,
        })
    }

    /// Return entry-local visible ranges intersecting a visual-line viewport.
    pub fn visible_ranges(
        &self,
        top_line: usize,
        viewport_height: usize,
    ) -> Vec<TranscriptVisibleRange> {
        let mut ranges = Vec::new();
        self.visible_ranges_into(top_line, viewport_height, &mut ranges);
        ranges
    }

    /// Write entry-local visible ranges intersecting a visual-line viewport.
    pub fn visible_ranges_into(
        &self,
        top_line: usize,
        viewport_height: usize,
        ranges: &mut Vec<TranscriptVisibleRange>,
    ) {
        ranges.clear();
        let bottom_line = top_line.saturating_add(viewport_height);
        for entry in &self.entries {
            let entry_end = entry.start_line.saturating_add(entry.display_lines);
            if entry_end <= top_line {
                continue;
            }
            if entry.start_line >= bottom_line {
                break;
            }
            let entry_line_start = top_line.saturating_sub(entry.start_line);
            let visual_line_start = entry.start_line.saturating_add(entry_line_start);
            let line_count = bottom_line.min(entry_end).saturating_sub(visual_line_start);
            ranges.push(TranscriptVisibleRange {
                entry_index: entry.entry_index,
                entry_line_start,
                line_count,
                visual_line_start,
                body_lines: entry.body_lines,
                has_separator: entry.has_separator,
            });
        }
    }

    fn entry_for_visual_line(&self, visual_line: usize) -> Option<TranscriptIndexedEntry> {
        let index = self.entries.partition_point(|entry| {
            entry.start_line.saturating_add(entry.display_lines) <= visual_line
        });
        let entry = *self.entries.get(index)?;
        (visual_line < entry.start_line.saturating_add(entry.display_lines)).then_some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_entry(text: &str) -> harness_core::UiTranscriptEntry {
        harness_core::UiTranscriptEntry::Text(text.to_string())
    }

    fn logical_line_count(entry: &harness_core::UiTranscriptEntry) -> usize {
        match entry {
            harness_core::UiTranscriptEntry::Text(text) => text.lines().count().max(1),
            harness_core::UiTranscriptEntry::SessionRecord(_) => 0,
        }
    }

    #[test]
    fn store_assigns_stable_ids_and_revisions() {
        let mut store = TranscriptStore::from_entries(vec![text_entry("one")]);
        let first = store.metadata_at(0).unwrap().id;
        let second = store.push_line("two".to_string());

        assert_eq!(store.index_of(first), Some(0));
        assert_eq!(store.index_of(second), Some(1));
        assert_eq!(store.metadata_at(1).unwrap().revision, 0);

        store.append_to_entry_with(second, " plus", |_| {}).unwrap();

        assert_eq!(store.entries()[1], text_entry("two plus"));
        assert_eq!(store.metadata_at(1).unwrap().revision, 1);
        assert_eq!(store.total_bytes(), "one".len() + "two plus".len());
    }

    #[test]
    fn store_prepend_and_drop_keep_metadata_in_lockstep() {
        let mut store = TranscriptStore::from_entries(vec![text_entry("current")]);
        let current = store.metadata_at(0).unwrap().id;
        let older = store.prepend_entries(vec![text_entry("older-a"), text_entry("older-b")]);

        assert_eq!(store.index_of(older[0]), Some(0));
        assert_eq!(store.index_of(older[1]), Some(1));
        assert_eq!(store.index_of(current), Some(2));
        assert_eq!(store.drop_oldest(), Some(older[0]));
        assert_eq!(store.drop_newest(), Some(current));
        assert_eq!(store.entries(), &[text_entry("older-b")]);
    }

    #[test]
    fn store_insert_and_truncate_keep_metadata_in_lockstep() {
        let mut store = TranscriptStore::from_entries(vec![
            text_entry("first"),
            text_entry("second"),
            text_entry("third"),
        ]);
        let first = store.metadata_at(0).unwrap().id;
        let second = store.metadata_at(1).unwrap().id;
        let third = store.metadata_at(2).unwrap().id;

        let inserted = store
            .insert_after(first, "inserted".to_string())
            .expect("insert after first");

        assert_eq!(store.index_of(first), Some(0));
        assert_eq!(store.index_of(inserted), Some(1));
        assert_eq!(store.index_of(second), Some(2));
        assert_eq!(store.index_of(third), Some(3));
        assert_eq!(store.truncate_after(inserted), Some(vec![second, third]));
        assert_eq!(
            store.entries(),
            &[text_entry("first"), text_entry("inserted")]
        );
    }

    #[test]
    fn line_index_counts_body_lines_and_inter_entry_separators() {
        let entries = vec![text_entry("one"), text_entry(""), text_entry("two\nthree")];

        let index = TranscriptLineIndex::build(&entries, logical_line_count);

        assert_eq!(index.total_lines(), 6);
        assert_eq!(index.entry_has_separator(0), true);
        assert_eq!(index.entry_has_separator(1), true);
        assert_eq!(index.entry_has_separator(2), false);
        assert_eq!(
            index.line_address(4),
            Some(TranscriptLineAddress {
                entry_index: 2,
                entry_line: 0,
                visual_line: 4,
                body_lines: 2,
                has_separator: false,
            })
        );
    }

    #[test]
    fn visible_ranges_are_entry_local() {
        let entries = vec![text_entry("a\nb"), text_entry("c"), text_entry("d\ne")];
        let index = TranscriptLineIndex::build(&entries, logical_line_count);

        let ranges = index.visible_ranges(2, 3);

        assert_eq!(
            ranges,
            vec![
                TranscriptVisibleRange {
                    entry_index: 0,
                    entry_line_start: 2,
                    line_count: 1,
                    visual_line_start: 2,
                    body_lines: 2,
                    has_separator: true,
                },
                TranscriptVisibleRange {
                    entry_index: 1,
                    entry_line_start: 0,
                    line_count: 2,
                    visual_line_start: 3,
                    body_lines: 1,
                    has_separator: true,
                },
            ]
        );
    }

    #[test]
    fn test_error_coalescing() {
        let mut store = TranscriptStore::from_entries(vec![]);
        store.push_line("error: first failure".to_string());
        store.push_line("error: first failure".to_string());
        store.push_line("error: second failure".to_string());
        store.push_line("error: first failure".to_string());
        store.push_line("error: first failure".to_string());

        assert_eq!(
            store.entries(),
            &[
                text_entry("error: first failure (x2)"),
                text_entry("error: second failure"),
                text_entry("error: first failure (x2)")
            ]
        );
    }
}
