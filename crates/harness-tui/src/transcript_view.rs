use std::collections::{HashMap, hash_map::Entry};

use crossterm::event::MouseEvent;
use harness_core::{UiSnapshot, sessions::TranscriptPage};
use ratatui::{layout::Rect, text::Line};

use crate::{
    MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_ENTRIES, TRANSCRIPT_PAGE_LINE_LIMIT,
    TranscriptLineMetadata, TranscriptRenderMetrics, TranscriptViewportPosition,
    TranscriptViewportRef, TuiAction, chars_range, selection_range_for_line,
    transcript::{
        TranscriptEntryId, TranscriptEntryMeta, TranscriptLineIndex, TranscriptStore,
        TranscriptVisibleRange,
    },
    transcript_content_width, transcript_content_width_with_scrollbar,
    transcript_entries_have_separator, transcript_entry_content_lines,
    transcript_entry_line_count_with_context, transcript_entry_lines_with_context,
    transcript_entry_render_context, transcript_scrollbar_visible,
    transcript_suffix_prefix_overlap, transcript_total_line_count, transcript_viewport_position,
    trim_transcript_entry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptRetention {
    PreserveTail,
    PreserveHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TranscriptSelectionPoint {
    pub(crate) visual_line: usize,
    pub(crate) content_col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptSelection {
    pub(crate) anchor: TranscriptSelectionPoint,
    pub(crate) cursor: TranscriptSelectionPoint,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptView {
    pub(crate) store: TranscriptStore,
    pub(crate) render_cache: TranscriptRenderCache,
    streaming_assistant_entry: Option<TranscriptEntryId>,
    pub(crate) scroll_offset: usize,
    pub(crate) last_view_height: usize,
    last_view_width: u16,
    pub(crate) page_before_seq: Option<u64>,
    pub(crate) history_reached_start: bool,
    pub(crate) page_loading: bool,
    pub(crate) selection: Option<TranscriptSelection>,
    pub(crate) last_area: Rect,
}

impl TranscriptView {
    pub(crate) fn from_snapshot(snapshot: &mut UiSnapshot) -> Self {
        Self {
            store: TranscriptStore::from_entries(std::mem::take(&mut snapshot.transcript_entries)),
            render_cache: TranscriptRenderCache::default(),
            streaming_assistant_entry: None,
            scroll_offset: 0,
            last_view_height: 10,
            last_view_width: 80,
            page_before_seq: None,
            history_reached_start: false,
            page_loading: false,
            selection: None,
            last_area: Rect::default(),
        }
    }

    pub(crate) fn materialize_snapshot(&self, snapshot: &UiSnapshot) -> UiSnapshot {
        let mut snapshot = snapshot.clone();
        snapshot.transcript_entries = self.store.entries().to_vec();
        snapshot
    }

    pub(crate) fn into_snapshot(self, mut snapshot: UiSnapshot) -> UiSnapshot {
        snapshot.transcript_entries = self.store.into_entries();
        snapshot
    }

    pub(crate) fn entries(&self) -> &[harness_core::UiTranscriptEntry] {
        self.store.entries()
    }

    pub fn entry_id_at(&self, index: usize) -> Option<TranscriptEntryId> {
        self.store.metadata_at(index).map(|metadata| metadata.id())
    }

    fn streaming_assistant_index(&self) -> Option<usize> {
        self.streaming_assistant_entry
            .and_then(|entry_id| self.store.index_of(entry_id))
    }

    pub(crate) fn begin_response_stream(&mut self) {
        self.streaming_assistant_entry = None;
    }

    pub(crate) fn complete_response_stream(&mut self) {
        self.streaming_assistant_entry = None;
    }

    fn page_size(&self) -> usize {
        self.last_view_height.saturating_sub(1).max(1)
    }

    pub(crate) fn scroll_up_page(&mut self) {
        self.scroll_up_by(self.page_size());
    }

    pub(crate) fn scroll_up_by(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
        self.clamp_scroll();
    }

    pub(crate) fn scroll_down_page(&mut self) {
        self.scroll_down_by(self.page_size());
    }

    pub(crate) fn scroll_down_by(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    pub(crate) fn scroll_to_top(&mut self) {
        self.scroll_offset = self.max_scroll_offset();
    }

    pub(crate) fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    pub(crate) fn request_page_if_needed(&mut self) -> Option<TuiAction> {
        if self.history_reached_start || self.page_loading {
            return None;
        }
        if self.scroll_offset < self.max_scroll_offset() {
            return None;
        }
        self.page_loading = true;
        Some(TuiAction::LoadTranscriptPage {
            before_seq: self.page_before_seq,
            max_lines: TRANSCRIPT_PAGE_LINE_LIMIT,
        })
    }

    pub(crate) fn update_render_metrics(&mut self, metrics: TranscriptRenderMetrics) {
        self.last_view_height = metrics.viewport_height;
        self.last_view_width = metrics.transcript_area.width;
        self.scroll_offset = metrics.scroll_offset;
        self.last_area = metrics.transcript_area;
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    pub(crate) fn selection(&self) -> Option<&TranscriptSelection> {
        self.selection.as_ref()
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    fn mouse_in_area(&self, mouse: MouseEvent) -> bool {
        let area = self.last_area;
        mouse.column >= area.x
            && mouse.column < area.x.saturating_add(area.width)
            && mouse.row >= area.y
            && mouse.row < area.y.saturating_add(area.height)
    }

    pub(crate) fn begin_mouse_selection(&mut self, mouse: MouseEvent) {
        if let Some(point) = self.selection_point_for_mouse(mouse, true) {
            self.selection = Some(TranscriptSelection {
                anchor: point,
                cursor: point,
            });
        } else if self.mouse_in_area(mouse) {
            self.selection = None;
        }
    }

    pub(crate) fn drag_mouse_selection(&mut self, mouse: MouseEvent) -> Option<TuiAction> {
        let action = self.auto_scroll_selection(mouse);
        self.update_selection_cursor_from_mouse(mouse);
        action
    }

    pub(crate) fn finish_mouse_selection(&mut self, mouse: MouseEvent) {
        self.update_selection_cursor_from_mouse(mouse);
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.anchor == selection.cursor)
        {
            self.selection = None;
        }
    }

    fn update_selection_cursor_from_mouse(&mut self, mouse: MouseEvent) {
        if self.selection.is_none() {
            return;
        }
        if let Some(point) = self.selection_point_for_mouse(mouse, false)
            && let Some(selection) = &mut self.selection
        {
            selection.cursor = point;
        }
    }

    fn selection_point_for_mouse(
        &mut self,
        mouse: MouseEvent,
        require_content_column: bool,
    ) -> Option<TranscriptSelectionPoint> {
        if !self.mouse_in_area(mouse) {
            return None;
        }
        let content_start = self.last_area.x.saturating_add(3);
        if require_content_column && mouse.column < content_start {
            return None;
        }
        let local_row = usize::from(mouse.row.saturating_sub(self.last_area.y));
        let viewport_position = self.viewport_position(self.last_view_height, self.last_view_width);
        let visual_line = viewport_position.top_line.saturating_add(local_row);
        let content = self.content_line_at(visual_line)?;
        let content_len = content.chars().count();
        let content_col = usize::from(mouse.column.saturating_sub(content_start)).min(content_len);
        Some(TranscriptSelectionPoint {
            visual_line,
            content_col,
        })
    }

    fn auto_scroll_selection(&mut self, mouse: MouseEvent) -> Option<TuiAction> {
        if !self.mouse_in_area(mouse) || self.last_area.height == 0 {
            return None;
        }
        let local_row = mouse.row.saturating_sub(self.last_area.y);
        if local_row <= 1 {
            self.scroll_up_by(1);
            return self.request_page_if_needed();
        }
        if local_row.saturating_add(2) >= self.last_area.height {
            self.scroll_down_by(1);
        }
        None
    }

    pub(crate) fn append_line(&mut self, line: String) {
        self.apply_tail_mutation(|view| {
            view.push_line_inner(line);
        });
    }
    pub(crate) fn append_entry(&mut self, entry: harness_core::UiTranscriptEntry) {
        self.apply_tail_mutation(|view| {
            view.push_entry_inner(entry);
        });
    }

    pub(crate) fn append_assistant_delta(&mut self, delta: &str) {
        self.apply_tail_mutation(|view| {
            view.append_assistant_delta_inner(delta);
        });
    }

    pub fn replace_entry_text(
        &mut self,
        entry_id: TranscriptEntryId,
        mut line: String,
    ) -> Option<()> {
        trim_transcript_entry(&mut line);
        self.selection = None;
        self.store.replace_entry(entry_id, line)?;
        self.clear_streaming_entry_if_missing();
        self.clamp_scroll();
        Some(())
    }

    pub fn insert_entry_after(
        &mut self,
        entry_id: TranscriptEntryId,
        mut line: String,
    ) -> Option<TranscriptEntryId> {
        trim_transcript_entry(&mut line);
        self.selection = None;
        let inserted_id = self.store.insert_after(entry_id, line)?;
        self.clamp_scroll();
        Some(inserted_id)
    }

    pub fn truncate_after_entry(
        &mut self,
        entry_id: TranscriptEntryId,
    ) -> Option<Vec<TranscriptEntryId>> {
        self.selection = None;
        let removed = self.store.truncate_after(entry_id)?;
        self.clear_streaming_entry_if_missing();
        self.clamp_scroll();
        Some(removed)
    }

    pub(crate) fn apply_page(&mut self, page: TranscriptPage) {
        self.page_loading = false;
        if let Some(next_before_seq) = page.next_before_seq {
            self.page_before_seq = Some(next_before_seq);
        }
        self.history_reached_start = page.reached_start;

        let mut incoming = page
            .lines
            .into_iter()
            .map(|line| harness_core::UiTranscriptEntry::SessionRecord(line.kind))
            .collect::<Vec<_>>();
        if incoming.is_empty() {
            return;
        }

        let overlap = transcript_suffix_prefix_overlap(&incoming, self.entries());
        let added_entries = incoming.len().saturating_sub(overlap);
        if added_entries == 0 {
            return;
        }
        self.selection = None;
        incoming.truncate(added_entries);
        let added_visual_lines = transcript_total_line_count(&incoming, self.current_wrap_width())
            + usize::from(!incoming.is_empty() && !self.store.is_empty());
        self.store.prepend_entries(incoming);
        self.scroll_offset = self.scroll_offset.saturating_add(added_visual_lines);
        self.enforce_memory_limit_with(TranscriptRetention::PreserveHead);
        self.clamp_scroll();
    }

    fn apply_tail_mutation(&mut self, mutation: impl FnOnce(&mut Self)) {
        let was_following_tail = self.scroll_offset == 0;
        let before_total = if was_following_tail {
            None
        } else {
            Some(self.total_lines())
        };
        mutation(self);
        self.after_tail_mutation(before_total, was_following_tail);
    }

    fn append_assistant_delta_inner(&mut self, delta: &str) {
        self.selection = None;
        let entry_id = match self.streaming_assistant_entry {
            Some(entry_id) if self.store.index_of(entry_id).is_some() => entry_id,
            _ => {
                let entry_id = self.push_line_inner("assistant: ".to_string());
                self.streaming_assistant_entry = Some(entry_id);
                entry_id
            }
        };
        self.store
            .append_to_entry_with(entry_id, delta, trim_transcript_entry)
            .expect("streaming assistant entry is present");
    }

    fn push_line_inner(&mut self, mut line: String) -> TranscriptEntryId {
        self.selection = None;
        trim_transcript_entry(&mut line);
        self.store.push_line(line)
    }
    fn push_entry_inner(&mut self, entry: harness_core::UiTranscriptEntry) -> TranscriptEntryId {
        self.selection = None;
        self.store.push_entry(entry)
    }

    pub(crate) fn enforce_memory_limit(&mut self) {
        self.enforce_memory_limit_with(TranscriptRetention::PreserveTail);
    }

    fn enforce_memory_limit_with(&mut self, retention: TranscriptRetention) {
        self.store.trim_lines(trim_transcript_entry);
        while self.store.len() > MAX_TRANSCRIPT_ENTRIES
            || self.store.total_bytes() > MAX_TRANSCRIPT_BYTES
        {
            if self.store.is_empty() {
                break;
            }
            match retention {
                TranscriptRetention::PreserveTail => self.drop_oldest_entry(),
                TranscriptRetention::PreserveHead => self.drop_newest_entry(),
            }
        }
    }

    fn after_tail_mutation(&mut self, before_total: Option<usize>, was_following_tail: bool) {
        self.enforce_memory_limit_with(TranscriptRetention::PreserveTail);
        if was_following_tail {
            self.scroll_offset = 0;
            return;
        }
        let before_total = before_total.expect("non-tail transcript mutation records total lines");
        let after_total = self.total_lines();
        if after_total > before_total {
            self.scroll_offset = self
                .scroll_offset
                .saturating_add(after_total - before_total);
        }
        self.clamp_scroll();
    }

    fn drop_oldest_entry(&mut self) {
        if let Some(removed_id) = self.store.drop_oldest()
            && self.streaming_assistant_entry == Some(removed_id)
        {
            self.streaming_assistant_entry = None;
        }
    }

    fn drop_newest_entry(&mut self) {
        if let Some(removed_id) = self.store.drop_newest()
            && self.streaming_assistant_entry == Some(removed_id)
        {
            self.streaming_assistant_entry = None;
        }
    }

    fn clear_streaming_entry_if_missing(&mut self) {
        if self
            .streaming_assistant_entry
            .is_some_and(|entry_id| self.store.index_of(entry_id).is_none())
        {
            self.streaming_assistant_entry = None;
        }
    }

    fn total_lines(&mut self) -> usize {
        let wrap_width = self.current_wrap_width();
        self.render_cache
            .line_index(&self.store, wrap_width)
            .total_lines()
    }

    fn max_scroll_offset(&mut self) -> usize {
        let viewport_height = self.last_view_height;
        self.total_lines().saturating_sub(viewport_height.max(1))
    }

    fn clamp_scroll(&mut self) {
        let max_scroll_offset = self.max_scroll_offset();
        self.scroll_offset = self.scroll_offset.min(max_scroll_offset);
    }

    fn viewport_position(
        &mut self,
        viewport_height: usize,
        viewport_width: u16,
    ) -> TranscriptViewportPosition {
        let scroll_offset = self.scroll_offset;
        let wrap_width = self.wrap_width_for_viewport(viewport_height, viewport_width);
        transcript_viewport_position(
            self.render_cache.line_index(&self.store, wrap_width),
            viewport_height,
            scroll_offset,
        )
    }

    fn content_line_at(&mut self, visual_line: usize) -> Option<String> {
        let wrap_width = self.current_wrap_width();
        let address = {
            let line_index = self.render_cache.line_index(&self.store, wrap_width);
            line_index.line_address(visual_line)?
        };
        if address.entry_line >= address.body_lines {
            return None;
        }
        let defer_syntax_highlighting =
            self.streaming_assistant_index() == Some(address.entry_index);
        self.render_cache
            .rendered_entry(
                &self.store,
                address.entry_index,
                defer_syntax_highlighting,
                wrap_width,
            )
            .content_lines(
                &self.store,
                address.entry_index,
                defer_syntax_highlighting,
                wrap_width,
            )
            .get(address.entry_line)
            .cloned()
    }

    fn current_wrap_width(&mut self) -> usize {
        self.wrap_width_for_viewport(self.last_view_height, self.last_view_width)
    }

    fn wrap_width_for_viewport(&mut self, viewport_height: usize, viewport_width: u16) -> usize {
        let wrap_width = transcript_content_width(viewport_width);
        let line_index = self.render_cache.line_index(&self.store, wrap_width);
        if transcript_scrollbar_visible(line_index, viewport_height, viewport_width) {
            transcript_content_width_with_scrollbar(viewport_width)
        } else {
            wrap_width
        }
    }

    pub(crate) fn selected_text(&mut self) -> Option<String> {
        let selection = self.selection.clone()?;
        selected_transcript_text_from_view(self, &selection)
    }

    pub(crate) fn viewport_ref(
        &mut self,
        viewport_height: usize,
        viewport_width: u16,
        include_metadata_content: bool,
    ) -> TranscriptViewportRef<'_> {
        let defer_syntax_highlighting_entry = self.streaming_assistant_index();
        transcript_viewport_from_store(
            &self.store,
            &mut self.render_cache,
            viewport_height,
            viewport_width,
            self.scroll_offset,
            defer_syntax_highlighting_entry,
            include_metadata_content,
        )
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptRenderCache {
    pub(crate) entries: HashMap<TranscriptEntryId, TranscriptRenderedEntry>,
    line_counts: HashMap<TranscriptEntryLineCountKey, usize>,
    pub(crate) line_indexes: HashMap<usize, TranscriptCachedLineIndex>,
    viewport_lines: Vec<Line<'static>>,
    viewport_metadata: Vec<TranscriptLineMetadata>,
    visible_ranges: Vec<TranscriptVisibleRange>,
    retained_revision: Option<u64>,
}

impl TranscriptRenderCache {
    fn line_index(&mut self, store: &TranscriptStore, wrap_width: usize) -> &TranscriptLineIndex {
        let revision = store.revision();
        if self
            .line_indexes
            .get(&wrap_width)
            .is_none_or(|cached| cached.revision != revision)
        {
            let index = self.build_line_index(store, wrap_width);
            self.line_indexes.insert(
                wrap_width,
                TranscriptCachedLineIndex {
                    revision,
                    wrap_width,
                    index,
                },
            );
        }
        &self
            .line_indexes
            .get(&wrap_width)
            .expect("transcript line index cache is initialized")
            .index
    }

    fn build_line_index(
        &mut self,
        store: &TranscriptStore,
        wrap_width: usize,
    ) -> TranscriptLineIndex {
        TranscriptLineIndex::build_with_index_and_separator(
            store.entries(),
            |index, entry| {
                let metadata = store.metadata()[index];
                let render_context = transcript_entry_render_context(store.entries(), index);
                let key = TranscriptEntryLineCountKey {
                    id: metadata.id(),
                    revision: metadata.revision(),
                    render_context: render_context.clone(),
                    wrap_width,
                };
                match self.line_counts.entry(key) {
                    Entry::Occupied(mut occupied) => *occupied.get_mut(),
                    Entry::Vacant(vacant) => *vacant.insert(
                        transcript_entry_line_count_with_context(entry, render_context, wrap_width),
                    ),
                }
            },
            |entry_index, next_visible_entry_index| {
                transcript_entries_have_separator(
                    store.entries(),
                    entry_index,
                    next_visible_entry_index,
                )
            },
        )
    }

    fn rendered_entry(
        &mut self,
        store: &TranscriptStore,
        index: usize,
        defer_syntax_highlighting: bool,
        wrap_width: usize,
    ) -> &mut TranscriptRenderedEntry {
        let entry = &store.entries()[index];
        let metadata = store.metadata()[index];
        let render_key =
            transcript_entry_render_key(store, index, defer_syntax_highlighting, wrap_width);
        match self.entries.entry(metadata.id()) {
            Entry::Occupied(mut occupied) => {
                if occupied.get().render_key != render_key {
                    occupied.insert(rendered_transcript_entry(
                        entry,
                        store.entries(),
                        index,
                        render_key,
                        defer_syntax_highlighting,
                    ));
                }
                occupied.into_mut()
            }
            Entry::Vacant(vacant) => vacant.insert(rendered_transcript_entry(
                entry,
                store.entries(),
                index,
                render_key,
                defer_syntax_highlighting,
            )),
        }
    }

    fn retain_current_entries(&mut self, store: &TranscriptStore) {
        let revision = store.revision();
        if self.retained_revision == Some(revision) {
            return;
        }
        self.entries
            .retain(|entry_id, _| store.index_of(*entry_id).is_some());
        self.line_counts
            .retain(|key, _| store.index_of(key.id).is_some());
        self.line_indexes
            .retain(|_, cached| cached.revision == revision);
        self.retained_revision = Some(revision);
    }

    #[cfg(test)]
    pub(crate) fn cached_line_count_entries(&self) -> usize {
        self.line_counts.len()
    }

    #[cfg(test)]
    pub(crate) fn cached_line_count_revision(&self, entry_id: TranscriptEntryId) -> Option<u64> {
        self.line_counts
            .keys()
            .find(|key| key.id == entry_id)
            .map(|key| key.revision)
    }

    #[cfg(test)]
    pub(crate) fn cached_line_index_revision(&self, wrap_width: usize) -> Option<u64> {
        self.line_indexes
            .get(&wrap_width)
            .map(|cached| cached.revision)
    }

    #[cfg(test)]
    pub(crate) fn cached_line_index_count(&self) -> usize {
        self.line_indexes.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TranscriptEntryLineCountKey {
    id: TranscriptEntryId,
    revision: u64,
    render_context: crate::TranscriptEntryRenderContext,
    wrap_width: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptCachedLineIndex {
    pub(crate) revision: u64,
    pub(crate) wrap_width: usize,
    index: TranscriptLineIndex,
}

fn rendered_transcript_entry(
    entry: &harness_core::UiTranscriptEntry,
    entries: &[harness_core::UiTranscriptEntry],
    index: usize,
    render_key: TranscriptEntryRenderKey,
    defer_syntax_highlighting: bool,
) -> TranscriptRenderedEntry {
    let mut render_context = transcript_entry_render_context(entries, index);
    render_context.defer_syntax_highlighting = defer_syntax_highlighting;
    TranscriptRenderedEntry {
        render_key,
        body_lines: transcript_entry_lines_with_context(
            entry,
            render_context,
            render_key.wrap_width,
        ),
        content_lines: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptRenderedEntry {
    pub(crate) render_key: TranscriptEntryRenderKey,
    body_lines: Vec<Line<'static>>,
    content_lines: Option<Vec<String>>,
}

impl TranscriptRenderedEntry {
    fn content_lines(
        &mut self,
        store: &TranscriptStore,
        index: usize,
        defer_syntax_highlighting: bool,
        wrap_width: usize,
    ) -> &[String] {
        if self.content_lines.is_none() {
            let entry = &store.entries()[index];
            let mut render_context = transcript_entry_render_context(store.entries(), index);
            render_context.defer_syntax_highlighting = defer_syntax_highlighting;
            self.content_lines = Some(transcript_entry_content_lines(
                entry,
                render_context,
                wrap_width,
            ));
        }
        self.content_lines
            .as_deref()
            .expect("transcript content lines are initialized")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptEntryRenderKey {
    revision: u64,
    previous: Option<TranscriptEntryContextVersion>,
    next: Option<TranscriptEntryContextVersion>,
    defer_syntax_highlighting: bool,
    wrap_width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptEntryContextVersion {
    id: TranscriptEntryId,
    revision: u64,
}

fn transcript_viewport_from_store<'a>(
    store: &TranscriptStore,
    render_cache: &'a mut TranscriptRenderCache,
    viewport_height: usize,
    viewport_width: u16,
    scroll_offset: usize,
    defer_syntax_highlighting_entry: Option<usize>,
    include_metadata_content: bool,
) -> TranscriptViewportRef<'a> {
    render_cache.retain_current_entries(store);
    let wrap_width = transcript_content_width(viewport_width);
    let line_index = render_cache.line_index(store, wrap_width);
    let wrap_width = if transcript_scrollbar_visible(line_index, viewport_height, viewport_width) {
        transcript_content_width_with_scrollbar(viewport_width)
    } else {
        wrap_width
    };
    let position = {
        let line_index = render_cache.line_index(store, wrap_width);
        transcript_viewport_position(line_index, viewport_height, scroll_offset)
    };
    let mut visible_ranges = std::mem::take(&mut render_cache.visible_ranges);
    let mut lines = std::mem::take(&mut render_cache.viewport_lines);
    let mut metadata = std::mem::take(&mut render_cache.viewport_metadata);
    {
        let line_index = render_cache.line_index(store, wrap_width);
        line_index.visible_ranges_into(
            position.top_line,
            position.viewport_height,
            &mut visible_ranges,
        );
    }
    lines.clear();
    metadata.clear();
    for visible_range in &visible_ranges {
        let index = visible_range.entry_index;
        let rendered_entry = render_cache.rendered_entry(
            store,
            index,
            defer_syntax_highlighting_entry == Some(index),
            wrap_width,
        );
        let entry_lines = rendered_entry.body_lines.len();
        let end = visible_range
            .entry_line_start
            .saturating_add(visible_range.line_count);
        let body_take = end
            .min(entry_lines)
            .saturating_sub(visible_range.entry_line_start);
        for offset in 0..body_take {
            let entry_display_line = visible_range.entry_line_start.saturating_add(offset);
            let content = if include_metadata_content {
                rendered_entry
                    .content_lines(
                        store,
                        index,
                        defer_syntax_highlighting_entry == Some(index),
                        wrap_width,
                    )
                    .get(entry_display_line)
                    .cloned()
            } else {
                None
            };
            let line = rendered_entry.body_lines[entry_display_line].clone();
            metadata.push(TranscriptLineMetadata {
                visual_line: visible_range.visual_line_start.saturating_add(offset),
                content,
            });
            lines.push(line);
        }
        if visible_range.has_separator
            && visible_range.entry_line_start <= entry_lines
            && end > entry_lines
        {
            metadata.push(TranscriptLineMetadata {
                visual_line: visible_range
                    .visual_line_start
                    .saturating_add(entry_lines.saturating_sub(visible_range.entry_line_start)),
                content: None,
            });
            lines.push(Line::from(String::new()));
        }
    }
    render_cache.visible_ranges = visible_ranges;
    render_cache.viewport_lines = lines;
    render_cache.viewport_metadata = metadata;

    TranscriptViewportRef {
        lines: &render_cache.viewport_lines,
        metadata: &render_cache.viewport_metadata,
        total_lines: position.total_lines,
        viewport_height: position.viewport_height,
        top_line: position.top_line,
        scroll_offset: position.scroll_offset,
    }
}

fn selected_transcript_text_from_view(
    transcript_view: &mut TranscriptView,
    selection: &TranscriptSelection,
) -> Option<String> {
    let (start, end) = crate::normalized_selection_points(selection);
    if start == end {
        return None;
    }
    let mut lines = Vec::new();
    for visual_line in start.visual_line..=end.visual_line {
        match transcript_view.content_line_at(visual_line) {
            Some(content) => {
                let content_len = content.chars().count();
                let (range_start, range_end) =
                    selection_range_for_line(selection, visual_line, content_len)
                        .unwrap_or((0, content_len));
                lines.push(chars_range(&content, range_start, range_end));
            }
            None => lines.push(String::new()),
        }
    }
    Some(lines.join("\n"))
}

fn transcript_entry_render_key(
    store: &TranscriptStore,
    index: usize,
    defer_syntax_highlighting: bool,
    wrap_width: usize,
) -> TranscriptEntryRenderKey {
    let metadata = store.metadata()[index];
    TranscriptEntryRenderKey {
        revision: metadata.revision(),
        previous: index
            .checked_sub(1)
            .and_then(|previous| store.metadata().get(previous))
            .map(transcript_entry_context_version),
        next: store
            .metadata()
            .get(index.saturating_add(1))
            .map(transcript_entry_context_version),
        defer_syntax_highlighting,
        wrap_width,
    }
}

fn transcript_entry_context_version(
    metadata: &TranscriptEntryMeta,
) -> TranscriptEntryContextVersion {
    TranscriptEntryContextVersion {
        id: metadata.id(),
        revision: metadata.revision(),
    }
}
