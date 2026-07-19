//! Transcript storage, projection, layout, scrolling, and selection.

mod document;
mod layout;
mod presentation;

use std::{fmt, ops::Range};

use document::TranscriptDocument;
#[cfg(test)]
use document::TranscriptEntry;
pub(crate) use document::TranscriptEntryId;
use layout::{TranscriptLayoutCache, TranscriptLineReference};

use crate::{
    display::{ClipboardText, LaidOutLine},
    domain::{PersistedTranscriptEntry, TranscriptPayload, TranscriptSnapshotEntry},
};

/// Stable semantic position in selectable transcript text.
///
/// Fields are private so only transcript projection and hit testing can
/// construct a selectable byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptPosition {
    entry: TranscriptEntryId,
    byte: usize,
}

/// Stable transcript selection endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptSelection {
    /// Fixed selection endpoint.
    pub(crate) anchor: TranscriptPosition,
    /// Moving selection endpoint.
    pub(crate) cursor: TranscriptPosition,
}

/// Direction of a transcript selection drag beyond a viewport edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptScrollDirection {
    /// Move toward older transcript content.
    Older,
    /// Move toward newer transcript content.
    Newer,
}

/// Stable identity for one viewport row across reflow and document mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptViewportAnchor {
    EntryLine {
        entry: TranscriptEntryId,
        projection_byte: usize,
    },
    SeparatorAfter {
        entry: TranscriptEntryId,
    },
}

/// Stable viewport mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewportState {
    FollowingTail,
    Anchored(TranscriptViewportAnchor),
}

/// Runtime assistant-stream state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssistantStream {
    Idle,
    Active { entry: Option<TranscriptEntryId> },
}

/// Persisted history request state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageRequestState {
    Idle,
    Loading { before_sequence: Option<u64> },
    ReachedStart,
    Rejected,
}

/// Transcript state-transition failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptError {
    /// A response stream started while another stream remained active.
    StreamAlreadyActive,
    /// A text delta arrived without an active stream.
    DeltaOutsideStream,
    /// A stream completion arrived without an active stream.
    CompletionOutsideStream,
    /// A transcript edit referenced an unknown stable entry.
    UnknownEntry(TranscriptEntryId),
    /// A loaded page contains duplicate source sequence numbers with
    /// conflicting payloads.
    ConflictingSequence(u64),
    /// A historical page arrived without a matching request.
    UnexpectedPageResponse,
    /// A non-terminal historical page did not advance toward older sequences.
    PageCursorDidNotAdvance {
        requested_before: Option<u64>,
        next_before_sequence: Option<u64>,
    },
    /// A non-terminal historical page contained no previously unseen entries.
    PageContainsNoNewEntries,
}

impl fmt::Display for TranscriptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StreamAlreadyActive => formatter.write_str("assistant stream is already active"),
            Self::DeltaOutsideStream => {
                formatter.write_str("assistant text delta arrived outside a response stream")
            }
            Self::CompletionOutsideStream => {
                formatter.write_str("response stream completed while no stream was active")
            }
            Self::UnknownEntry(entry) => {
                write!(formatter, "unknown transcript entry {}", entry.get())
            }
            Self::ConflictingSequence(sequence) => {
                write!(
                    formatter,
                    "transcript sequence {sequence} has conflicting payloads"
                )
            }
            Self::UnexpectedPageResponse => {
                formatter.write_str("transcript page arrived without a matching request")
            }
            Self::PageCursorDidNotAdvance {
                requested_before,
                next_before_sequence,
            } => write!(
                formatter,
                "transcript page cursor did not advance from {requested_before:?} to {next_before_sequence:?}"
            ),
            Self::PageContainsNoNewEntries => {
                formatter.write_str("transcript page contained no new entries")
            }
        }
    }
}

impl std::error::Error for TranscriptError {}

/// One transcript line prepared for a viewport.
#[derive(Debug, Clone)]
pub(crate) enum TranscriptViewportLine {
    /// A wrapped line belonging to a stable transcript entry.
    Entry {
        /// Stable transcript entry.
        entry: TranscriptEntryId,
        /// Validated terminal-cell layout.
        line: LaidOutLine,
        /// Selected local byte range applied by the backend.
        selection: Option<Range<usize>>,
    },
    /// Synthetic non-selectable separator between entries.
    Separator,
}

/// Prepared transcript viewport and hit-test metadata.
#[derive(Debug, Clone)]
pub(crate) struct TranscriptViewport {
    /// Visible transcript lines in screen order.
    pub(crate) lines: Vec<TranscriptViewportLine>,
    /// Total wrapped transcript lines.
    pub(crate) total_lines: usize,
    /// Absolute first visible wrapped line.
    pub(crate) top_line: usize,
    /// Effective viewport height.
    pub(crate) height: usize,
}

impl TranscriptViewport {
    /// Resolves a visible terminal cell to a stable selectable position.
    pub(crate) fn position_at(&self, row: usize, cell: usize) -> Option<TranscriptPosition> {
        let TranscriptViewportLine::Entry { entry, line, .. } = self.lines.get(row)? else {
            return None;
        };
        line.selection_position(cell)
            .map(|byte| TranscriptPosition {
                entry: *entry,
                byte,
            })
    }
}

/// Transcript aggregate with exclusive ownership of every transcript concern.
#[derive(Debug)]
pub(crate) struct Transcript {
    document: TranscriptDocument,
    layout: TranscriptLayoutCache,
    viewport: ViewportState,
    selection: Option<TranscriptSelection>,
    stream: AssistantStream,
    page_request: PageRequestState,
    before_sequence: Option<u64>,
}

impl Transcript {
    /// Imports complete initial transcript state.
    pub(crate) fn import(
        entries: Vec<TranscriptSnapshotEntry>,
        response_streaming: bool,
    ) -> Result<Self, TranscriptError> {
        let before_sequence = entries.iter().filter_map(|entry| entry.sequence).min();
        Ok(Self {
            document: TranscriptDocument::from_snapshot(entries)?,
            layout: TranscriptLayoutCache::default(),
            viewport: ViewportState::FollowingTail,
            selection: None,
            stream: if response_streaming {
                AssistantStream::Active { entry: None }
            } else {
                AssistantStream::Idle
            },
            page_request: PageRequestState::Idle,
            before_sequence,
        })
    }

    #[cfg(test)]
    /// Returns entries in chronological order.
    pub(crate) fn entries(&self) -> impl Iterator<Item = &TranscriptEntry> {
        self.document.entries()
    }

    /// Returns whether a non-empty transcript selection exists.
    pub(crate) fn has_selection(&self) -> bool {
        self.selection
            .is_some_and(|selection| selection.anchor != selection.cursor)
    }

    /// Starts a response stream.
    pub(crate) fn begin_response_stream(&mut self) -> Result<(), TranscriptError> {
        if self.stream != AssistantStream::Idle {
            return Err(TranscriptError::StreamAlreadyActive);
        }
        self.stream = AssistantStream::Active { entry: None };
        Ok(())
    }

    /// Appends an assistant delta to the active stream.
    pub(crate) fn append_assistant_delta(
        &mut self,
        delta: crate::domain::ExternalText,
    ) -> Result<(), TranscriptError> {
        let AssistantStream::Active { entry } = self.stream else {
            return Err(TranscriptError::DeltaOutsideStream);
        };
        let entry = match entry {
            Some(entry) => {
                self.document.append_assistant_text(entry, &delta)?;
                entry
            }
            None => {
                let entry = self.document.push(TranscriptPayload::Message {
                    role: crate::domain::MessageRole::Assistant,
                    text: delta,
                });
                self.stream = AssistantStream::Active { entry: Some(entry) };
                entry
            }
        };
        self.layout.invalidate_entry(entry);
        self.after_tail_mutation();
        Ok(())
    }

    /// Completes the active response stream.
    pub(crate) fn complete_response_stream(&mut self) -> Result<(), TranscriptError> {
        if self.stream == AssistantStream::Idle {
            return Err(TranscriptError::CompletionOutsideStream);
        }
        self.stream = AssistantStream::Idle;
        Ok(())
    }

    /// Appends one semantic transcript payload.
    pub(crate) fn append(&mut self, payload: TranscriptPayload) -> TranscriptEntryId {
        let entry = self.document.push(payload);
        self.after_tail_mutation();
        entry
    }

    /// Marks a historical page request as in flight when the retained start is
    /// visible in the current viewport geometry.
    pub(crate) fn request_older_page(
        &mut self,
        width: u16,
        height: usize,
    ) -> Option<crate::domain::RuntimeRequest> {
        self.layout.prepare(&self.document, width);
        if self.resolve_top_line(height) == 0 && matches!(self.page_request, PageRequestState::Idle)
        {
            let before_sequence = self.before_sequence;
            self.page_request = PageRequestState::Loading { before_sequence };
            return Some(crate::domain::RuntimeRequest::LoadTranscriptPage { before_sequence });
        }
        None
    }

    /// Applies one sequence-numbered historical page.
    pub(crate) fn apply_page(
        &mut self,
        entries: Vec<PersistedTranscriptEntry>,
        next_before_sequence: Option<u64>,
        reached_start: bool,
    ) -> Result<(), TranscriptError> {
        let PageRequestState::Loading { before_sequence } = self.page_request else {
            self.page_request = PageRequestState::Rejected;
            return Err(TranscriptError::UnexpectedPageResponse);
        };
        let novel = match self.document.novel_page_entries(entries) {
            Ok(novel) => novel,
            Err(error) => {
                self.page_request = PageRequestState::Rejected;
                return Err(error);
            }
        };

        if !reached_start {
            let cursor_advanced = match (before_sequence, next_before_sequence) {
                (None, Some(_)) => true,
                (Some(before), Some(next)) => next < before,
                (_, None) => false,
            };
            if !cursor_advanced {
                self.page_request = PageRequestState::Rejected;
                return Err(TranscriptError::PageCursorDidNotAdvance {
                    requested_before: before_sequence,
                    next_before_sequence,
                });
            }
            if novel.is_empty() {
                self.page_request = PageRequestState::Rejected;
                return Err(TranscriptError::PageContainsNoNewEntries);
            }
        }

        if !novel.is_empty() {
            self.document.prepend_page(novel);
            self.layout.invalidate_document();
        }
        self.before_sequence = next_before_sequence;
        self.page_request = if reached_start {
            PageRequestState::ReachedStart
        } else {
            PageRequestState::Idle
        };
        Ok(())
    }

    /// Marks a failed page delivery so the request may be retried.
    pub(crate) fn page_delivery_failed(&mut self) {
        if matches!(self.page_request, PageRequestState::Loading { .. }) {
            self.page_request = PageRequestState::Idle;
        }
    }

    /// Scrolls by wrapped lines. Positive values move toward older content.
    pub(crate) fn scroll_by(&mut self, width: u16, height: usize, delta: isize) {
        self.layout.prepare(&self.document, width);
        let current_top = self.resolve_top_line(height);
        let maximum_top = self.layout.total_lines().saturating_sub(height.max(1));
        let target_top = if delta >= 0 {
            current_top.saturating_sub(delta as usize)
        } else {
            current_top
                .saturating_add(delta.unsigned_abs())
                .min(maximum_top)
        };
        self.set_top_line(target_top, height);
    }

    /// Scrolls an active selection by one row and extends it at the exposed edge.
    pub(crate) fn scroll_selection(
        &mut self,
        width: u16,
        height: usize,
        direction: TranscriptScrollDirection,
        cell: usize,
    ) {
        let delta = match direction {
            TranscriptScrollDirection::Older => 1,
            TranscriptScrollDirection::Newer => -1,
        };
        self.scroll_by(width, height, delta);
        let height = height.max(1);
        let top_line = self.resolve_top_line(height);
        let edge_line = match direction {
            TranscriptScrollDirection::Older => top_line,
            TranscriptScrollDirection::Newer => top_line
                .saturating_add(height.saturating_sub(1))
                .min(self.layout.total_lines().saturating_sub(1)),
        };
        if let Some(position) = self
            .layout
            .selection_position_near_line(edge_line, cell, direction)
        {
            self.extend_selection(position);
        }
    }

    /// Sets the absolute first visible wrapped line.
    pub(crate) fn scroll_to_line(&mut self, width: u16, height: usize, top_line: usize) {
        self.layout.prepare(&self.document, width);
        self.set_top_line(top_line, height);
    }

    /// Scrolls to the oldest retained line.
    pub(crate) fn scroll_to_top(&mut self, width: u16, height: usize) {
        self.layout.prepare(&self.document, width);
        self.set_top_line(0, height);
    }

    /// Follows the newest transcript tail.
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.viewport = ViewportState::FollowingTail;
    }

    /// Builds the current transcript viewport.
    pub(crate) fn viewport(&mut self, width: u16, height: usize) -> TranscriptViewport {
        self.layout.prepare(&self.document, width);
        let height = height.max(1);
        let top_line = self.resolve_top_line(height);
        let end = top_line
            .saturating_add(height)
            .min(self.layout.total_lines());
        let mut lines = Vec::with_capacity(end.saturating_sub(top_line));

        for reference in self.layout.references(top_line..end) {
            match *reference {
                TranscriptLineReference::SeparatorAfter { .. } => {
                    lines.push(TranscriptViewportLine::Separator);
                }
                TranscriptLineReference::Entry { entry, line_index } => {
                    let line = self
                        .layout
                        .entry_line(entry, line_index)
                        .expect("layout reference resolves to a cached entry line")
                        .clone();
                    let selection = self.selection_range_for_entry(entry);
                    lines.push(TranscriptViewportLine::Entry {
                        entry,
                        line,
                        selection,
                    });
                }
            }
        }

        TranscriptViewport {
            lines,
            total_lines: self.layout.total_lines(),
            top_line,
            height,
        }
    }

    /// Starts transcript selection at a stable position.
    pub(crate) fn begin_selection(&mut self, position: TranscriptPosition) {
        self.selection = Some(TranscriptSelection {
            anchor: position,
            cursor: position,
        });
    }

    /// Extends transcript selection to a stable position.
    pub(crate) fn extend_selection(&mut self, position: TranscriptPosition) {
        if let Some(selection) = &mut self.selection {
            selection.cursor = position;
        }
    }

    /// Completes selection and removes a collapsed range.
    pub(crate) fn finish_selection(&mut self) {
        if self
            .selection
            .is_some_and(|selection| selection.anchor == selection.cursor)
        {
            self.selection = None;
        }
    }

    /// Clears transcript selection.
    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Returns selected control-free transcript text.
    pub(crate) fn selected_text(&mut self) -> Option<ClipboardText> {
        let selection = self.selection?;
        let (start, end) = self.normalized_selection(selection)?;
        let start_index = self.document.index_of(start.entry)?;
        let end_index = self.document.index_of(end.entry)?;
        let mut output = String::new();

        for index in start_index..=end_index {
            if index > start_index {
                output.push('\n');
            }
            let entry = self.document.entry_at(index)?;
            let text = self.layout.selection_text(&self.document, entry.id());
            let range_start = if entry.id() == start.entry {
                start.byte.min(text.len())
            } else {
                0
            };
            let range_end = if entry.id() == end.entry {
                end.byte.min(text.len())
            } else {
                text.len()
            };
            if range_start <= range_end
                && text.is_char_boundary(range_start)
                && text.is_char_boundary(range_end)
            {
                output.push_str(&text[range_start..range_end]);
            }
        }

        ClipboardText::from_control_free(output)
    }

    /// Consumes the transcript and returns semantic entries in display order.
    /// Consumes the transcript and returns snapshot entries in display order.
    pub(crate) fn into_snapshot_entries(self) -> Vec<TranscriptSnapshotEntry> {
        self.document.into_snapshot_entries()
    }

    fn after_tail_mutation(&mut self) {
        self.layout.invalidate_document();
    }

    fn resolve_top_line(&self, height: usize) -> usize {
        let maximum_top = self.layout.total_lines().saturating_sub(height.max(1));
        match self.viewport {
            ViewportState::FollowingTail => maximum_top,
            ViewportState::Anchored(anchor) => self
                .layout
                .line_for_anchor(anchor)
                .unwrap_or(maximum_top)
                .min(maximum_top),
        }
    }

    fn set_top_line(&mut self, top_line: usize, height: usize) {
        let maximum_top = self.layout.total_lines().saturating_sub(height.max(1));
        if top_line >= maximum_top {
            self.viewport = ViewportState::FollowingTail;
            return;
        }
        if let Some(anchor) = self.layout.anchor_for_line(top_line) {
            self.viewport = ViewportState::Anchored(anchor);
        }
    }

    #[cfg(test)]
    fn test_position(&mut self, entry: TranscriptEntryId, byte: usize) -> TranscriptPosition {
        let text = self.layout.selection_text(&self.document, entry);
        assert!(byte <= text.len());
        assert!(text.is_char_boundary(byte));
        TranscriptPosition { entry, byte }
    }

    fn normalized_selection(
        &self,
        selection: TranscriptSelection,
    ) -> Option<(TranscriptPosition, TranscriptPosition)> {
        let anchor_index = self.document.index_of(selection.anchor.entry)?;
        let cursor_index = self.document.index_of(selection.cursor.entry)?;
        if (anchor_index, selection.anchor.byte) <= (cursor_index, selection.cursor.byte) {
            Some((selection.anchor, selection.cursor))
        } else {
            Some((selection.cursor, selection.anchor))
        }
    }

    fn selection_range_for_entry(&self, entry: TranscriptEntryId) -> Option<Range<usize>> {
        let selection = self.selection?;
        let (start, end) = self.normalized_selection(selection)?;
        let index = self.document.index_of(entry)?;
        let start_index = self.document.index_of(start.entry)?;
        let end_index = self.document.index_of(end.entry)?;
        if index < start_index || index > end_index {
            return None;
        }
        let start_byte = if entry == start.entry { start.byte } else { 0 };
        let end_byte = if entry == end.entry {
            end.byte
        } else {
            usize::MAX
        };
        (start_byte < end_byte).then_some(start_byte..end_byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ExternalText, MessageRole, TranscriptSnapshotEntry};

    fn message(text: &str) -> TranscriptPayload {
        TranscriptPayload::Message {
            role: MessageRole::Assistant,
            text: ExternalText::new(text),
        }
    }

    #[test]
    fn stream_machine_rejects_invalid_event_order() {
        let mut transcript = Transcript::import(Vec::new(), false).unwrap();
        assert_eq!(
            transcript.append_assistant_delta(ExternalText::new("bad")),
            Err(TranscriptError::DeltaOutsideStream)
        );
        transcript.begin_response_stream().unwrap();
        assert_eq!(
            transcript.begin_response_stream(),
            Err(TranscriptError::StreamAlreadyActive)
        );
        transcript
            .append_assistant_delta(ExternalText::new("hello"))
            .unwrap();
        transcript.complete_response_stream().unwrap();
        assert_eq!(
            transcript.complete_response_stream(),
            Err(TranscriptError::CompletionOutsideStream)
        );
    }

    #[test]
    fn page_identity_uses_sequences_instead_of_content() {
        let mut transcript = Transcript::import(
            vec![TranscriptSnapshotEntry {
                sequence: None,
                payload: message("same"),
            }],
            false,
        )
        .unwrap();
        let page = vec![
            PersistedTranscriptEntry {
                sequence: 1,
                payload: message("same"),
            },
            PersistedTranscriptEntry {
                sequence: 2,
                payload: message("same"),
            },
        ];

        assert!(transcript.request_older_page(80, 20).is_some());
        transcript.apply_page(page, Some(1), false).unwrap();

        assert_eq!(transcript.entries().count(), 3);
    }

    #[test]
    fn selection_is_stable_across_width_changes() {
        let mut transcript = Transcript::import(
            vec![TranscriptSnapshotEntry {
                sequence: None,
                payload: message("hello world"),
            }],
            false,
        )
        .unwrap();
        let entry = transcript.entries().next().unwrap().id();
        let start = transcript.test_position(entry, 0);
        let end = transcript.test_position(entry, 5);
        transcript.begin_selection(start);
        transcript.extend_selection(end);
        let _ = transcript.viewport(5, 3);
        let narrow = transcript.selected_text().unwrap();
        let _ = transcript.viewport(80, 3);
        let wide = transcript.selected_text().unwrap();
        assert_eq!(narrow.as_str(), "hello");
        assert_eq!(wide.as_str(), "hello");
    }
    #[test]
    fn short_transcript_requests_history_when_its_start_is_visible() {
        let mut transcript = Transcript::import(
            vec![TranscriptSnapshotEntry {
                sequence: None,
                payload: message("short"),
            }],
            false,
        )
        .unwrap();

        transcript.scroll_to_top(80, 20);
        assert!(matches!(
            transcript.request_older_page(80, 20),
            Some(crate::domain::RuntimeRequest::LoadTranscriptPage { .. })
        ));
        assert!(transcript.request_older_page(80, 20).is_none());
    }

    #[test]
    fn initial_sequence_identity_sets_the_older_page_cursor() {
        let mut transcript = Transcript::import(
            vec![
                TranscriptSnapshotEntry {
                    sequence: Some(12),
                    payload: message("twelve"),
                },
                TranscriptSnapshotEntry {
                    sequence: Some(14),
                    payload: message("fourteen"),
                },
            ],
            false,
        )
        .unwrap();

        assert!(matches!(
            transcript.request_older_page(80, 20),
            Some(crate::domain::RuntimeRequest::LoadTranscriptPage {
                before_sequence: Some(12),
            })
        ));
    }

    #[test]
    fn wrapped_entry_scroll_progress_is_stable_across_preparation() {
        let mut transcript = Transcript::import(
            vec![TranscriptSnapshotEntry {
                sequence: None,
                payload: message("abcdefghijklmnopqrstuvwxyz0123456789"),
            }],
            false,
        )
        .unwrap();
        let width = 5;
        let height = 2;
        let maximum_top = transcript.viewport(width, height).top_line;
        assert!(maximum_top > 2);

        let mut expected_top = maximum_top;
        while expected_top > 0 {
            expected_top -= 1;
            transcript.scroll_by(width, height, 1);
            for _ in 0..4 {
                assert_eq!(transcript.viewport(width, height).top_line, expected_top);
            }
        }

        while expected_top < maximum_top {
            expected_top += 1;
            transcript.scroll_by(width, height, -1);
            for _ in 0..4 {
                assert_eq!(transcript.viewport(width, height).top_line, expected_top);
            }
        }
    }

    #[test]
    fn separator_remains_the_top_row_across_preparation() {
        let mut transcript = Transcript::import(
            vec![
                TranscriptSnapshotEntry {
                    sequence: None,
                    payload: message("first"),
                },
                TranscriptSnapshotEntry {
                    sequence: None,
                    payload: message("second"),
                },
            ],
            false,
        )
        .unwrap();

        transcript.scroll_by(80, 1, 1);
        for _ in 0..4 {
            let viewport = transcript.viewport(80, 1);
            assert_eq!(viewport.top_line, 1);
            assert!(matches!(
                viewport.lines.first(),
                Some(TranscriptViewportLine::Separator)
            ));
        }
    }

    #[test]
    fn width_change_uses_the_row_start_at_an_exact_wrap_boundary() {
        let mut transcript = Transcript::import(
            vec![TranscriptSnapshotEntry {
                sequence: None,
                payload: message("abcdefghijkl"),
            }],
            false,
        )
        .unwrap();

        transcript.scroll_by(3, 1, 2);
        let narrow = transcript.viewport(3, 1);
        assert_eq!(narrow.top_line, 2);
        assert_eq!(viewport_first_line_text(&narrow), "efg");

        let wide = transcript.viewport(6, 1);
        assert_eq!(wide.top_line, 1);
        assert_eq!(viewport_first_line_text(&wide), "efghij");
    }

    #[test]
    fn anchored_content_survives_zero_one_and_wider_widths() {
        let mut transcript = Transcript::import(
            vec![TranscriptSnapshotEntry {
                sequence: None,
                payload: message("abcdefghijklmnop"),
            }],
            false,
        )
        .unwrap();

        transcript.scroll_by(4, 1, 2);
        assert!(viewport_first_line_text(&transcript.viewport(4, 1)).contains('g'));

        let zero = transcript.viewport(0, 1);
        assert!(viewport_first_line_text(&zero).contains('g'));
        let one = transcript.viewport(1, 1);
        assert_eq!(
            viewport_first_line_text(&one),
            viewport_first_line_text(&zero)
        );
        let wider = transcript.viewport(8, 1);
        assert!(viewport_first_line_text(&wider).contains('g'));
    }

    #[test]
    fn prepending_history_preserves_content_and_exposes_the_new_page() {
        let mut transcript = Transcript::import(
            vec![
                TranscriptSnapshotEntry {
                    sequence: Some(10),
                    payload: message("ten"),
                },
                TranscriptSnapshotEntry {
                    sequence: Some(12),
                    payload: message("twelve"),
                },
            ],
            false,
        )
        .unwrap();
        let retained_oldest = transcript.entries().next().unwrap().id();

        transcript.scroll_to_top(80, 1);
        assert!(transcript.request_older_page(80, 1).is_some());
        transcript
            .apply_page(
                vec![PersistedTranscriptEntry {
                    sequence: 8,
                    payload: message("eight"),
                }],
                Some(8),
                false,
            )
            .unwrap();

        let preserved = transcript.viewport(80, 1);
        assert_eq!(preserved.top_line, 2);
        assert!(matches!(
            preserved.lines.first(),
            Some(TranscriptViewportLine::Entry { entry, .. })
                if *entry == retained_oldest
        ));

        transcript.scroll_by(80, 1, 1);
        let separator = transcript.viewport(80, 1);
        assert_eq!(separator.top_line, 1);
        assert!(matches!(
            separator.lines.first(),
            Some(TranscriptViewportLine::Separator)
        ));
        assert!(transcript.request_older_page(80, 1).is_none());

        transcript.scroll_by(80, 1, 1);
        let older = transcript.viewport(80, 1);
        assert_eq!(older.top_line, 0);
        assert_eq!(viewport_first_line_text(&older), "• eight");
        assert!(matches!(
            transcript.request_older_page(80, 1),
            Some(crate::domain::RuntimeRequest::LoadTranscriptPage {
                before_sequence: Some(8)
            })
        ));
    }

    #[test]
    fn invalid_nonterminal_pages_cannot_restart_paging() {
        let mut unchanged_cursor = Transcript::import(
            vec![TranscriptSnapshotEntry {
                sequence: Some(10),
                payload: message("ten"),
            }],
            false,
        )
        .unwrap();
        assert!(unchanged_cursor.request_older_page(80, 20).is_some());
        assert_eq!(
            unchanged_cursor.apply_page(Vec::new(), Some(10), false),
            Err(TranscriptError::PageCursorDidNotAdvance {
                requested_before: Some(10),
                next_before_sequence: Some(10),
            })
        );
        assert!(unchanged_cursor.request_older_page(80, 20).is_none());

        let mut empty_page = Transcript::import(
            vec![TranscriptSnapshotEntry {
                sequence: Some(10),
                payload: message("ten"),
            }],
            false,
        )
        .unwrap();
        assert!(empty_page.request_older_page(80, 20).is_some());
        assert_eq!(
            empty_page.apply_page(Vec::new(), Some(9), false),
            Err(TranscriptError::PageContainsNoNewEntries)
        );
        assert!(empty_page.request_older_page(80, 20).is_none());
    }

    fn viewport_first_line_text(viewport: &TranscriptViewport) -> String {
        let Some(TranscriptViewportLine::Entry { line, .. }) = viewport.lines.first() else {
            return String::new();
        };
        line.runs().iter().map(|run| run.text()).collect()
    }

    #[test]
    fn conflicting_startup_sequence_is_rejected() {
        let entries = vec![
            TranscriptSnapshotEntry {
                sequence: Some(7),
                payload: message("first"),
            },
            TranscriptSnapshotEntry {
                sequence: Some(7),
                payload: message("conflict"),
            },
        ];

        assert_eq!(
            Transcript::import(entries, false).unwrap_err(),
            TranscriptError::ConflictingSequence(7)
        );
    }

    #[test]
    fn identical_startup_sequence_is_idempotent() {
        let entry = TranscriptSnapshotEntry {
            sequence: Some(7),
            payload: message("same"),
        };

        let transcript = Transcript::import(vec![entry.clone(), entry], false).unwrap();

        assert_eq!(transcript.entries().count(), 1);
    }
}
