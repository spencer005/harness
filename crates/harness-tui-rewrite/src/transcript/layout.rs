//! Cached transcript projection and terminal-cell line index.

use std::{collections::HashMap, ops::Range};

use super::{
    TranscriptPosition, TranscriptScrollDirection, TranscriptViewportAnchor,
    document::{TranscriptDocument, TranscriptEntryId},
    presentation,
};
use crate::display::{ControlFree, DisplayDocument, DocumentLimits, LaidOut, LaidOutLine};

#[derive(Debug, Clone, Copy)]
pub(super) enum TranscriptLineReference {
    Entry {
        entry: TranscriptEntryId,
        line_index: usize,
    },
    SeparatorAfter {
        entry: TranscriptEntryId,
    },
}

#[derive(Debug, Clone)]
struct CachedProjection {
    revision: u64,
    document: DisplayDocument<ControlFree>,
    selection_text: String,
}

#[derive(Debug, Clone)]
struct CachedEntryLayout {
    revision: u64,
    width: u16,
    document: DisplayDocument<LaidOut>,
}

#[derive(Debug, Default)]
pub(super) struct TranscriptLayoutCache {
    projections: HashMap<TranscriptEntryId, CachedProjection>,
    entry_layouts: HashMap<TranscriptEntryId, CachedEntryLayout>,
    references: Vec<TranscriptLineReference>,
    indexed_document_revision: Option<u64>,
    indexed_width: Option<u16>,
}

impl TranscriptLayoutCache {
    pub(super) fn invalidate_entry(&mut self, entry: TranscriptEntryId) {
        self.projections.remove(&entry);
        self.entry_layouts.remove(&entry);
        self.indexed_document_revision = None;
    }

    pub(super) fn invalidate_document(&mut self) {
        self.indexed_document_revision = None;
    }

    pub(super) fn prepare(&mut self, document: &TranscriptDocument, width: u16) {
        let width = width.max(1);
        self.retain_current_entries(document);
        if self.indexed_document_revision == Some(document.revision())
            && self.indexed_width == Some(width)
        {
            return;
        }

        self.references.clear();
        let ids = document
            .entries()
            .map(|entry| entry.id())
            .collect::<Vec<_>>();
        for (index, id) in ids.iter().copied().enumerate() {
            self.prepare_entry(document, id, width);
            let line_count = self
                .entry_layouts
                .get(&id)
                .expect("entry layout is prepared")
                .document
                .lines()
                .len();
            self.references.extend((0..line_count).map(|line_index| {
                TranscriptLineReference::Entry {
                    entry: id,
                    line_index,
                }
            }));
            if index + 1 < ids.len() {
                self.references
                    .push(TranscriptLineReference::SeparatorAfter { entry: id });
            }
        }

        self.indexed_document_revision = Some(document.revision());
        self.indexed_width = Some(width);
    }

    pub(super) fn total_lines(&self) -> usize {
        self.references.len()
    }

    pub(super) fn references(&self, range: Range<usize>) -> &[TranscriptLineReference] {
        &self.references[range]
    }

    pub(super) fn entry_line(
        &self,
        entry: TranscriptEntryId,
        line_index: usize,
    ) -> Option<&LaidOutLine> {
        self.entry_layouts
            .get(&entry)?
            .document
            .lines()
            .get(line_index)
    }

    pub(super) fn selection_text(
        &mut self,
        document: &TranscriptDocument,
        entry: TranscriptEntryId,
    ) -> String {
        self.prepare_projection(document, entry);
        self.projections
            .get(&entry)
            .expect("projection is prepared")
            .selection_text
            .clone()
    }

    pub(super) fn selection_position_near_line(
        &self,
        visual_line: usize,
        cell: usize,
        direction: TranscriptScrollDirection,
    ) -> Option<TranscriptPosition> {
        let final_line = self.references.len().checked_sub(1)?;
        let visual_line = visual_line.min(final_line);
        let mut position_at = |line_index| {
            let TranscriptLineReference::Entry {
                entry,
                line_index: entry_line_index,
            } = self.references[line_index]
            else {
                return None;
            };
            self.entry_line(entry, entry_line_index)?
                .selection_position(cell)
                .map(|byte| TranscriptPosition { entry, byte })
        };
        match direction {
            TranscriptScrollDirection::Older => (0..=visual_line).rev().find_map(&mut position_at),
            TranscriptScrollDirection::Newer => (visual_line..=final_line).find_map(position_at),
        }
    }

    pub(super) fn line_for_anchor(&self, anchor: TranscriptViewportAnchor) -> Option<usize> {
        match anchor {
            TranscriptViewportAnchor::EntryLine {
                entry,
                projection_byte,
            } => {
                let matching_lines =
                    self.references
                        .iter()
                        .enumerate()
                        .filter_map(|(visual_line, reference)| {
                            let TranscriptLineReference::Entry {
                                entry: line_entry,
                                line_index,
                            } = *reference
                            else {
                                return None;
                            };
                            (line_entry == entry)
                                .then_some((visual_line, self.entry_line(line_entry, line_index)?))
                        });
                matching_lines
                    .clone()
                    .find_map(|(visual_line, line)| {
                        (line.projection_bytes().start == projection_byte).then_some(visual_line)
                    })
                    .or_else(|| {
                        matching_lines.clone().find_map(|(visual_line, line)| {
                            let range = line.projection_bytes();
                            (range.start < projection_byte && projection_byte < range.end)
                                .then_some(visual_line)
                        })
                    })
                    .or_else(|| {
                        matching_lines
                            .filter(|(_, line)| line.projection_bytes().end == projection_byte)
                            .map(|(visual_line, _)| visual_line)
                            .next_back()
                    })
            }
            TranscriptViewportAnchor::SeparatorAfter { entry } => {
                self.references.iter().position(|reference| {
                    matches!(
                        reference,
                        TranscriptLineReference::SeparatorAfter {
                            entry: separator_entry
                        } if *separator_entry == entry
                    )
                })
            }
        }
    }

    pub(super) fn anchor_for_line(&self, visual_line: usize) -> Option<TranscriptViewportAnchor> {
        let reference = *self.references.get(visual_line)?;
        match reference {
            TranscriptLineReference::Entry { entry, line_index } => {
                let projection_byte = self.entry_line(entry, line_index)?.projection_bytes().start;
                Some(TranscriptViewportAnchor::EntryLine {
                    entry,
                    projection_byte,
                })
            }
            TranscriptLineReference::SeparatorAfter { entry } => {
                Some(TranscriptViewportAnchor::SeparatorAfter { entry })
            }
        }
    }

    fn prepare_entry(&mut self, document: &TranscriptDocument, id: TranscriptEntryId, width: u16) {
        self.prepare_projection(document, id);
        let entry = document
            .entry(id)
            .expect("layout entry belongs to transcript");
        let revision = entry.revision();
        let current = self
            .entry_layouts
            .get(&id)
            .is_some_and(|cached| cached.revision == revision && cached.width == width);
        if current {
            return;
        }
        let projection = self
            .projections
            .get(&id)
            .expect("entry projection is prepared")
            .document
            .clone();
        let laid_out = match entry.payload() {
            crate::domain::TranscriptPayload::ToolOutput { .. } => {
                projection.bound(DocumentLimits::TOOL_OUTPUT).layout(width)
            }
            _ => projection.layout(width),
        };
        self.entry_layouts.insert(
            id,
            CachedEntryLayout {
                revision,
                width,
                document: laid_out,
            },
        );
    }

    fn prepare_projection(&mut self, document: &TranscriptDocument, id: TranscriptEntryId) {
        let entry = document
            .entry(id)
            .expect("projection entry belongs to transcript");
        let current = self
            .projections
            .get(&id)
            .is_some_and(|cached| cached.revision == entry.revision());
        if current {
            return;
        }
        let projection = presentation::project(entry.payload());
        let selection_text = projection.selectable_text();
        self.projections.insert(
            id,
            CachedProjection {
                revision: entry.revision(),
                document: projection,
                selection_text,
            },
        );
    }

    fn retain_current_entries(&mut self, document: &TranscriptDocument) {
        self.projections
            .retain(|id, _| document.index_of(*id).is_some());
        self.entry_layouts
            .retain(|id, _| document.index_of(*id).is_some());
    }
}
