//! Immutable frame preparation, rendering, and hit testing.

use std::time::Instant;

use crate::picker::SessionPickerState;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, StatefulWidget, Wrap},
};

use crate::{
    app::{Application, SlashCommandStatus},
    display::{
        DisplayDocument, DocumentLimits, LaidOut, RawDocumentBuilder, StyleId, backend_style,
        prepare_document, prepare_one_line_document,
    },
    domain::{AgentStatus, ContextUsage, ExternalText, SessionState},
    input::{PromptPosition, PromptViewport},
    transcript::{
        TranscriptPosition, TranscriptScrollDirection, TranscriptViewport, TranscriptViewportLine,
    },
};

const PROMPT_MARGIN_WIDTH: u16 = 2;
const MIN_PROMPT_HEIGHT: usize = 3;
const MAX_ACTIVITY_HEIGHT: usize = 3;
const MAX_PROMPT_PERCENT: usize = 40;
const SCROLLBAR_WIDTH: u16 = 1;

/// Areas allocated for one terminal frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameAreas {
    /// Transcript including its scrollbar column.
    pub(crate) transcript: Rect,
    /// Transcript text excluding its scrollbar column.
    pub(crate) transcript_content: Rect,
    /// Activity panel.
    pub(crate) activity: Rect,
    /// Complete prompt panel.
    pub(crate) prompt: Rect,
    /// Prompt text excluding its left margin.
    pub(crate) prompt_content: Rect,
    /// Status row.
    pub(crate) status: Rect,
    /// Left session list column when session picker is open.
    pub(crate) session_picker_list: Option<Rect>,
}

/// Scrollbar geometry shared by rendering and mouse hit testing.
#[derive(Debug, Clone, Copy)]
struct TranscriptScrollbar {
    area: Rect,
    maximum_top_line: usize,
    thumb_start: u16,
    thumb_height: u16,
}

impl TranscriptScrollbar {
    fn press(self, row: u16, current_top_line: usize) -> (usize, u16) {
        let relative_row = row.saturating_sub(self.area.y);
        let thumb_end = self.thumb_start.saturating_add(self.thumb_height);
        if (self.thumb_start..thumb_end).contains(&relative_row) {
            return (
                current_top_line,
                relative_row.saturating_sub(self.thumb_start),
            );
        }

        let thumb_offset = self.thumb_height / 2;
        (self.top_line_for_pointer(row, thumb_offset), thumb_offset)
    }

    fn top_line_for_pointer(self, row: u16, thumb_offset: u16) -> usize {
        let thumb_offset = thumb_offset.min(self.thumb_height.saturating_sub(1));
        let final_row = self.area.height.saturating_sub(1);
        let relative_row = row
            .clamp(
                self.area.y,
                self.area
                    .y
                    .saturating_add(self.area.height.saturating_sub(1)),
            )
            .saturating_sub(self.area.y)
            .min(final_row);
        let travel = self.area.height.saturating_sub(self.thumb_height);
        if travel == 0 {
            return 0;
        }
        let thumb_start = relative_row.saturating_sub(thumb_offset).min(travel);
        usize::from(thumb_start)
            .saturating_mul(self.maximum_top_line)
            .saturating_add(usize::from(travel) / 2)
            / usize::from(travel)
    }
}

/// Fully prepared immutable frame consumed by rendering and hit testing.
#[derive(Debug)]
pub(crate) struct PreparedFrame {
    areas: FrameAreas,
    transcript: TranscriptViewport,
    transcript_scrollbar: Option<TranscriptScrollbar>,
    prompt_viewport: PromptViewport,
    prompt_document: DisplayDocument<LaidOut>,
    activity_document: Option<DisplayDocument<LaidOut>>,
    status_document: DisplayDocument<LaidOut>,
    notice_document: Option<DisplayDocument<LaidOut>>,
    slash_status: SlashCommandStatus,
    prompt_cursor: Option<Position>,
    /// Cloned picker state when the overlay is open, None otherwise.
    pub(crate) picker_state: Option<SessionPickerState>,
    /// Cloned rewind picker state when the overlay is open, None otherwise.
    pub(crate) rewind_picker_state: Option<crate::picker::RewindPickerState>,
}

impl PreparedFrame {
    #[cfg(test)]
    /// Returns allocated frame areas.
    pub(crate) fn areas(&self) -> FrameAreas {
        self.areas
    }

    /// Resolves an in-prompt mouse point to a prompt grapheme boundary.
    /// Resolves an in-prompt mouse point to a prompt grapheme boundary.
    /// Resolves an in-prompt mouse point to a prompt grapheme boundary.
    pub(crate) fn prompt_position(&self, point: Position) -> Option<PromptPosition> {
        contains(self.areas.prompt_content, point).then(|| {
            let r = usize::from(point.y.saturating_sub(self.areas.prompt_content.y));
            let c = usize::from(point.x.saturating_sub(self.areas.prompt_content.x));
            self.prompt_viewport.position_at(r, c)
        })
    }

    /// Resolves any mouse point to a prompt boundary clamped to the prompt.
    pub(crate) fn prompt_position_clamped(&self, point: Position) -> Option<PromptPosition> {
        let area = self.areas.prompt_content;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let column = point.x.clamp(area.x, area.x.saturating_add(area.width.saturating_sub(1)));
        let r = point.y.clamp(area.y, area.y.saturating_add(area.height.saturating_sub(1)));
        Some(self.prompt_viewport.position_at(
            usize::from(r.saturating_sub(area.y)),
            usize::from(column.saturating_sub(area.x)),
        ))
    }

    /// Resolves a transcript mouse point to a stable selectable position.
    pub(crate) fn transcript_position(&self, point: Position) -> Option<TranscriptPosition> {
        if !contains(self.areas.transcript_content, point) {
            return None;
        }
        let r = usize::from(point.y.saturating_sub(self.areas.transcript_content.y));
        let c = usize::from(point.x.saturating_sub(self.areas.transcript_content.x));
        self.transcript.position_at(r, c)
    }

    /// Returns whether a mouse event is inside the transcript text area.
    pub(crate) fn transcript_contains(&self, point: Position) -> bool {
        contains(self.areas.transcript_content, point)
    }

    /// Resolves a mouse point clamped to the transcript viewport.
    pub(crate) fn transcript_position_clamped(
        &self,
        point: Position,
    ) -> Option<TranscriptPosition> {
        let area = self.areas.transcript_content;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let column = point.x.clamp(area.x, area.x.saturating_add(area.width.saturating_sub(1)));
        let r = point.y.clamp(area.y, area.y.saturating_add(area.height.saturating_sub(1)));
        self.transcript.position_at(
            usize::from(r.saturating_sub(area.y)),
            usize::from(column.saturating_sub(area.x)),
        )
    }

    /// Returns transcript selection scrolling at a viewport edge.
    ///
    /// A transcript at terminal row zero uses its first row as the upward scroll
    /// zone because Crossterm cannot report a negative pointer row.
    pub(crate) fn transcript_selection_scroll(
        &self,
        point: Position,
    ) -> Option<(TranscriptScrollDirection, usize)> {
        let area = self.areas.transcript_content;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let cell = usize::from(
            point.x.clamp(area.x, area.x.saturating_add(area.width.saturating_sub(1)))
                .saturating_sub(area.x),
        );
        let after_final_row = area.y.saturating_add(area.height);
        if point.y < area.y || (area.y == 0 && point.y == 0) {
            Some((TranscriptScrollDirection::Older, cell))
        } else if point.y >= after_final_row {
            Some((TranscriptScrollDirection::Newer, cell))
        } else {
            None
        }
    }

    /// Maps a scrollbar press to an absolute top line and stable thumb offset.
    pub(crate) fn transcript_scrollbar_press(&self, point: Position) -> Option<(usize, u16)> {
        let scrollbar = self.transcript_scrollbar?;
        contains(scrollbar.area, point)
            .then(|| scrollbar.press(point.y, self.transcript.top_line))
    }

    /// Maps any captured drag point to the nearest scrollbar track row.
    pub(crate) fn transcript_scrollbar_top_line_clamped(
        &self,
        point: Position,
        thumb_offset: u16,
    ) -> Option<usize> {
        self.transcript_scrollbar
            .map(|scrollbar| scrollbar.top_line_for_pointer(point.y, thumb_offset))
    }

    /// Returns prompt content width for visual movement.
    pub(crate) fn prompt_width(&self) -> u16 {
        self.areas.prompt_content.width
    }

    /// Returns transcript content dimensions for scrolling.
    pub(crate) fn transcript_dimensions(&self) -> (u16, usize) {
        (
            self.areas.transcript_content.width,
            usize::from(self.areas.transcript_content.height),
        )
    }
}

/// Prepares a frame and its hit-test map from current application state.
pub(crate) fn prepare(application: &mut Application, area: Rect, now: Instant) -> PreparedFrame {
    let provisional_prompt_width = area.width.saturating_sub(PROMPT_MARGIN_WIDTH);
    let prompt_metrics = application
        .prompt_mut()
        .layout_metrics(provisional_prompt_width);
    let activity_document = prepare_activity(
        application.session(),
        application.working_elapsed(now),
        area.width,
    );
    let activity_line_count = activity_document
        .as_ref()
        .map(|document| document.lines().len())
        .unwrap_or(0);
    let session_picker_open = application.session_picker.is_some();
    let areas = allocate_areas(area, prompt_metrics.line_count, activity_line_count, session_picker_open);
    let prompt_metrics = if areas.prompt_content.width == provisional_prompt_width {
        prompt_metrics
    } else {
        application
            .prompt_mut()
            .layout_metrics(areas.prompt_content.width)
    };
    let prompt_scroll = prompt_metrics
        .cursor_row
        .saturating_add(1)
        .saturating_sub(usize::from(areas.prompt_content.height));
    let prompt_viewport = application.prompt_mut().viewport(
        areas.prompt_content.width,
        prompt_scroll,
        usize::from(areas.prompt_content.height),
    );
    let slash_status = application.slash_command_status();
    let prompt_cursor = prompt_cursor_position(&prompt_viewport, areas.prompt_content);
    let prompt_document = prepare_prompt(&prompt_viewport, areas.prompt_content.width, &slash_status);
    let status_document = prepare_status(application.session(), areas.status.width);
    let notice_document = prepare_notice(application, area.width);
    let transcript = if let Some(picker) = &application.session_picker {
        let filtered = picker.filtered_sessions();
        let selected = picker.list_state.selected().and_then(|i| filtered.get(i));
        if let Some((session_meta, _)) = selected {
            let preview_initial = crate::domain::InitialState {
                session_id: crate::domain::ExternalText::new(session_meta.id.clone()),
                thread_title: crate::domain::ExternalText::new(session_meta.title.clone()),
                provider: None,
                model: crate::domain::ModelState {
                    model: crate::domain::ExternalText::new(session_meta.model.clone()),
                    reasoning_effort: None,
                    service_tier: None,
                },
                developer_mode: false,
                response_streaming: false,
                last_ttft_ms: None,
                transcript: session_meta
                    .initial_entries
                    .iter()
                    .map(|e| crate::domain::TranscriptSnapshotEntry {
                        sequence: e.sequence,
                        payload: crate::runtime::adapter::convert_payload(e.payload.clone()),
                    })
                    .collect(),
                prompt: String::new(),
                prompt_cursor: 0,
                queued_steering: None,
                agents: Vec::new(),
                active_activity_ids: Vec::new(),
            };
            if let Ok(mut preview_app) = Application::import(preview_initial) {
                preview_app.transcript_mut().viewport(
                    areas.transcript_content.width,
                    usize::from(areas.transcript_content.height),
                )
            } else {
                application.transcript_mut().viewport(
                    areas.transcript_content.width,
                    usize::from(areas.transcript_content.height),
                )
            }
        } else {
            application.transcript_mut().viewport(
                areas.transcript_content.width,
                usize::from(areas.transcript_content.height),
            )
        }
    } else {
        application.transcript_mut().viewport(
            areas.transcript_content.width,
            usize::from(areas.transcript_content.height),
        )
    };
    let transcript_scrollbar = prepare_transcript_scrollbar(areas, &transcript);

    PreparedFrame {
        areas,
        transcript,
        transcript_scrollbar,
        prompt_viewport,
        prompt_document,
        activity_document,
        status_document,
        notice_document,
        slash_status,
        prompt_cursor,
        picker_state: application.session_picker.clone(),
        rewind_picker_state: application.rewind_picker.clone(),
    }
}

/// Renders one immutable prepared frame.
pub(crate) fn render(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    render_transcript(frame, prepared);
    render_activity(frame, prepared);
    render_prompt(frame, prepared);
    render_status(frame, prepared);
    render_notice(frame, prepared);
    if prepared.picker_state.is_some() {
        render_session_picker_overlay(frame, prepared);
    } else if prepared.rewind_picker_state.is_some() {
        render_rewind_picker_overlay(frame, prepared);
    } else {
        render_slash_command_overlay(frame, prepared);
        if let Some(cursor) = prepared.prompt_cursor {
            frame.set_cursor_position(cursor);
        }
    }
}

fn allocate_areas(area: Rect, prompt_line_count: usize, activity_line_count: usize, session_picker_open: bool) -> FrameAreas {
    let (picker_rect, main_rect) = if session_picker_open && area.width >= 40 {
        let picker_width = (area.width * 30 / 100).max(25);
        let left = Rect {
            x: area.x,
            y: area.y,
            width: picker_width,
            height: area.height,
        };
        let right = Rect {
            x: area.x.saturating_add(picker_width),
            y: area.y,
            width: area.width.saturating_sub(picker_width),
            height: area.height,
        };
        (Some(left), right)
    } else {
        (None, area)
    };

    let total_height = usize::from(main_rect.height);
    let status_height = usize::from(total_height > 0);
    let available = total_height.saturating_sub(status_height);
    let transcript_reserve = usize::from(available > 0);
    let prompt_cap = available.saturating_sub(transcript_reserve);
    let percentage_cap = total_height
        .saturating_mul(MAX_PROMPT_PERCENT)
        .checked_div(100)
        .unwrap_or(0)
        .max(MIN_PROMPT_HEIGHT)
        .min(prompt_cap);
    let prompt_height = if prompt_cap == 0 {
        0
    } else {
        prompt_line_count
            .max(MIN_PROMPT_HEIGHT)
            .min(percentage_cap.max(1))
    };
    let activity_cap = available
        .saturating_sub(prompt_height)
        .saturating_sub(transcript_reserve);
    let activity_height = activity_line_count
        .min(MAX_ACTIVITY_HEIGHT)
        .min(activity_cap);
    let transcript_height = available
        .saturating_sub(prompt_height)
        .saturating_sub(activity_height);

    let transcript = Rect {
        x: main_rect.x,
        y: main_rect.y,
        width: main_rect.width,
        height: to_u16(transcript_height),
    };
    let activity = Rect {
        x: main_rect.x,
        y: transcript.y.saturating_add(transcript.height),
        width: main_rect.width,
        height: to_u16(activity_height),
    };
    let prompt = Rect {
        x: main_rect.x,
        y: activity.y.saturating_add(activity.height),
        width: main_rect.width,
        height: to_u16(prompt_height),
    };
    let status = Rect {
        x: main_rect.x,
        y: prompt.y.saturating_add(prompt.height),
        width: main_rect.width,
        height: to_u16(status_height),
    };
    let prompt_margin = prompt.width.min(PROMPT_MARGIN_WIDTH);
    let prompt_content = Rect {
        x: prompt.x.saturating_add(prompt_margin),
        y: prompt.y,
        width: prompt.width.saturating_sub(prompt_margin),
        height: prompt.height,
    };
    let scrollbar = if transcript.width > 1 {
        SCROLLBAR_WIDTH
    } else {
        0
    };
    let transcript_content = Rect {
        width: transcript.width.saturating_sub(scrollbar),
        ..transcript
    };

    FrameAreas {
        transcript,
        transcript_content,
        activity,
        prompt,
        prompt_content,
        status,
        session_picker_list: picker_rect,
    }
}

fn prepare_transcript_scrollbar(
    areas: FrameAreas,
    transcript: &TranscriptViewport,
) -> Option<TranscriptScrollbar> {
    if areas.transcript.width <= areas.transcript_content.width
        || areas.transcript.height == 0
        || transcript.total_lines <= transcript.height
    {
        return None;
    }
    let area = Rect {
        x: areas
            .transcript
            .x
            .saturating_add(areas.transcript.width.saturating_sub(1)),
        y: areas.transcript.y,
        width: SCROLLBAR_WIDTH,
        height: areas.transcript.height,
    };
    let maximum_top_line = transcript
        .total_lines
        .saturating_sub(transcript.height.max(1));
    let track_height = usize::from(area.height);
    let thumb_height = track_height
        .saturating_mul(transcript.height)
        .checked_div(transcript.total_lines)
        .unwrap_or(0)
        .max(1)
        .min(track_height);
    let travel = track_height.saturating_sub(thumb_height);
    let thumb_start = if maximum_top_line == 0 || travel == 0 {
        0
    } else {
        transcript.top_line.saturating_mul(travel) / maximum_top_line
    };
    Some(TranscriptScrollbar {
        area,
        maximum_top_line,
        thumb_start: to_u16(thumb_start),
        thumb_height: to_u16(thumb_height),
    })
}

fn prepare_prompt(
    prompt_viewport: &PromptViewport,
    width: u16,
    slash_status: &SlashCommandStatus,
) -> DisplayDocument<LaidOut> {
    let mut builder = RawDocumentBuilder::new();
    for (line_index, line) in prompt_viewport.lines().iter().enumerate() {
        if line_index > 0 {
            builder.line_break();
        }
        for run in line.runs() {
            let run_text = run.text();
            let style = if run.selected() {
                StyleId::Selection
            } else if let SlashCommandStatus::Matched {
                invoked_as,
                syntax_valid,
                ..
            } = slash_status
            {
                let cmd_token = format!("/{}", invoked_as);
                if run_text == cmd_token
                    || (run_text.starts_with(&cmd_token)
                        && (run_text.len() == cmd_token.len()
                            || run_text[cmd_token.len()..].starts_with(' ')))
                {
                    StyleId::Command
                } else if !syntax_valid {
                    StyleId::SyntaxError
                } else {
                    StyleId::Prompt
                }
            } else {
                StyleId::Prompt
            };

            if style == StyleId::Prompt {
                // Apply WYSIWYG markdown parsing for prompt input runs
                append_prompt_markdown(&mut builder, run_text);
            } else {
                builder.plain(run_text, style, true);
            }
        }
    }
    let (bytes, lines, cells) = prompt_viewport.projection_metrics();
    prepare_document(
        builder.build(),
        DocumentLimits::prompt_viewport(bytes, lines, cells),
        width,
    )
}

fn append_prompt_markdown(builder: &mut RawDocumentBuilder, text: &str) {
    let trimmed = text.trim_end_matches(['\r', '\n']);
    let is_heading = trimmed.starts_with('#');
    let base_style = if is_heading {
        StyleId::Heading
    } else {
        StyleId::Prompt
    };

    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut text_start = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'`' {
            if text_start < i {
                builder.plain(&trimmed[text_start..i], base_style, true);
            }
            let code_start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'`' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            builder.plain(&trimmed[code_start..i], StyleId::Code, true);
            text_start = i;
        } else if bytes[i] == b'*' {
            let count_start = i;
            while i < bytes.len() && bytes[i] == b'*' {
                i += 1;
            }
            let count = i - count_start;

            if text_start < count_start {
                builder.plain(&trimmed[text_start..count_start], base_style, true);
            }

            if count >= 4 {
                builder.plain(&trimmed[count_start..i], StyleId::Bold, true);
            } else if count == 2 {
                builder.plain("**", StyleId::Italic, true);
            } else if count == 1 {
                if let Some(close_rel) = trimmed[i..].find('*') {
                    let close_idx = i + close_rel;
                    builder.plain(&trimmed[count_start..=close_idx], StyleId::Bold, true);
                    i = close_idx + 1;
                } else {
                    builder.plain("*", base_style, true);
                }
            } else {
                builder.plain(&trimmed[count_start..i], base_style, true);
            }
            text_start = i;
        } else {
            i += 1;
        }
    }

    if text_start < trimmed.len() {
        builder.plain(&trimmed[text_start..], base_style, true);
    }
}

fn prepare_activity(
    session: &SessionState,
    working_elapsed: Option<std::time::Duration>,
    width: u16,
) -> Option<DisplayDocument<LaidOut>> {
    let mut builder = RawDocumentBuilder::new();
    let mut line_count = 0usize;

    if let Some(elapsed) = working_elapsed {
        builder.plain("• Working ", StyleId::Active, false);
        builder.plain(format!("({}s)", elapsed.as_secs()), StyleId::Muted, false);
        line_count += 1;
    }
    if let Some(queued) = session.queued_steering.as_ref()
        && line_count < MAX_ACTIVITY_HEIGHT
    {
        append_panel_line_break(&mut builder, line_count);
        builder.plain("queued ", StyleId::Muted, false);
        builder.plain(queued.as_str(), StyleId::Queued, true);
        builder.plain("  ·  Esc send now", StyleId::Muted, false);
        line_count += 1;
    }
    for activity in session.activities.values() {
        if line_count >= MAX_ACTIVITY_HEIGHT {
            break;
        }
        append_panel_line_break(&mut builder, line_count);
        builder.plain("activity ", StyleId::Muted, false);
        builder.plain(activity.description.as_str(), StyleId::Active, true);
        if let Some(detail) = activity.detail.as_ref() {
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(detail.as_str(), StyleId::Muted, true);
        }
        line_count += 1;
    }
    for agent in session.agents.values() {
        if line_count >= MAX_ACTIVITY_HEIGHT {
            break;
        }
        append_panel_line_break(&mut builder, line_count);
        builder.plain("agent ", StyleId::Muted, false);
        builder.plain(agent.path.as_str(), StyleId::Active, true);
        builder.plain(" ", StyleId::Muted, false);
        let (status, detail) = match &agent.status {
            AgentStatus::Running => ("running", agent.last_activity_message.as_ref()),
            AgentStatus::Waiting => ("waiting", agent.last_activity_message.as_ref()),
            AgentStatus::Completed(detail) => ("completed", Some(detail)),
            AgentStatus::Failed(detail) => ("failed", Some(detail)),
            AgentStatus::Interrupted => ("interrupted", agent.last_activity_message.as_ref()),
        };
        builder.plain(status, StyleId::Muted, false);
        if let Some(detail) = detail {
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(detail.as_str(), StyleId::Muted, true);
        }
        line_count += 1;
    }

    (line_count > 0).then(|| prepare_document(builder.build(), DocumentLimits::UI_LABEL, width))
}

fn append_panel_line_break(builder: &mut RawDocumentBuilder, line_count: usize) {
    if line_count > 0 {
        builder.line_break();
    }
}

fn prepare_status(session: &SessionState, width: u16) -> DisplayDocument<LaidOut> {
    let mut builder = RawDocumentBuilder::new();
    builder.plain("session: ", StyleId::Status, false);
    builder.plain(session.session_id.as_str(), StyleId::Status, true);
    builder.plain(" │ provider: ", StyleId::Status, false);
    if let Some(provider) = session.provider.as_ref() {
        builder.plain(provider.display_name.as_str(), StyleId::Status, true);
        builder.plain("/", StyleId::Status, false);
        builder.plain(provider.transport.label(), StyleId::Status, false);
    } else {
        builder.plain("—", StyleId::Status, false);
    }
    builder.plain(" │ model: ", StyleId::Status, false);
    builder.plain(session.model.model.as_str(), StyleId::Status, true);
    builder.plain(" · ", StyleId::Status, false);
    builder.plain(
        session
            .model
            .reasoning_effort
            .as_ref()
            .map(ExternalText::as_str)
            .unwrap_or("default"),
        StyleId::Status,
        true,
    );
    builder.plain(" · ", StyleId::Status, false);
    builder.plain(
        session
            .model
            .service_tier
            .as_ref()
            .map(ExternalText::as_str)
            .unwrap_or("default"),
        StyleId::Status,
        true,
    );
    builder.plain(" │ ctx: ", StyleId::Status, false);
    match session.context_usage {
        Some(usage) => {
            builder.plain(
                context_label(usage),
                if usage.needs_warning() {
                    StyleId::Queued
                } else {
                    StyleId::Status
                },
                false,
            );
        }
        None => {
            builder.plain("—", StyleId::Status, false);
        }
    }
    prepare_one_line_document(builder.build(), DocumentLimits::UI_LABEL, width)
}

fn prepare_notice(application: &Application, width: u16) -> Option<DisplayDocument<LaidOut>> {
    let mut builder = RawDocumentBuilder::new();
    let mut has_notice = false;
    if application.exit_armed() {
        builder.plain("press Ctrl-C again to exit", StyleId::Queued, false);
        has_notice = true;
    }
    has_notice.then(|| prepare_one_line_document(builder.build(), DocumentLimits::UI_LABEL, width))
}

fn prompt_cursor_position(viewport: &PromptViewport, area: Rect) -> Option<Position> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let (row, cell) = viewport.cursor_position()?;
    (row < usize::from(area.height)).then(|| Position {
        x: area
            .x
            .saturating_add(to_u16(cell).min(area.width.saturating_sub(1))),
        y: area.y.saturating_add(to_u16(row)),
    })
}

fn render_transcript(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    let area = prepared.areas.transcript_content;
    frame.render_widget(Paragraph::new(""), area);
    let lines = prepared
        .transcript
        .lines
        .iter()
        .map(|line| match line {
            TranscriptViewportLine::Entry {
                line, selection, ..
            } => DisplayDocument::<LaidOut>::ratatui_line(line, selection.as_ref()),
            TranscriptViewportLine::Separator => Line::default(),
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);

    if let Some(scrollbar) = prepared.transcript_scrollbar {
        let mut lines = vec![Line::default(); usize::from(scrollbar.area.height)];
        let thumb_end = scrollbar.thumb_start.saturating_add(scrollbar.thumb_height);
        for row in scrollbar.thumb_start..thumb_end {
            lines[usize::from(row)] = Line::from(Span::styled("█", backend_style(StyleId::Muted)));
        }
        frame.render_widget(Paragraph::new(lines), scrollbar.area);
    }
}

fn render_activity(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    let Some(document) = prepared.activity_document.as_ref() else {
        return;
    };
    if prepared.areas.activity.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(document.ratatui_lines()),
        prepared.areas.activity,
    );
}

fn render_prompt(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    frame.render_widget(
        Paragraph::new("").style(backend_style(StyleId::Prompt)),
        prepared.areas.prompt,
    );
    let margin = Rect {
        width: prepared.areas.prompt.width.min(PROMPT_MARGIN_WIDTH),
        ..prepared.areas.prompt
    };
    let margin_lines = (0..margin.height)
        .map(|_| {
            Line::from(Span::styled(
                if margin.width > 1 { "│ " } else { "│" },
                backend_style(StyleId::Muted),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(margin_lines).style(backend_style(StyleId::Prompt)),
        margin,
    );
    frame.render_widget(
        Paragraph::new(prepared.prompt_document.ratatui_lines())
            .style(backend_style(StyleId::Prompt)),
        prepared.areas.prompt_content,
    );
}

fn render_status(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    frame.render_widget(
        Paragraph::new(prepared.status_document.ratatui_lines())
            .style(backend_style(StyleId::Status)),
        prepared.areas.status,
    );
}

fn notice_area(document: &DisplayDocument<LaidOut>, transcript: Rect) -> Option<Rect> {
    let line = document.lines().first()?;
    let width = to_u16(line.width()).min(transcript.width);
    (width > 0 && transcript.height > 0).then(|| Rect {
        x: transcript
            .x
            .saturating_add(transcript.width.saturating_sub(width)),
        y: transcript.y,
        width,
        height: 1,
    })
}

fn render_notice(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    let Some(document) = prepared.notice_document.as_ref() else {
        return;
    };
    let Some(area) = notice_area(document, prepared.areas.transcript) else {
        return;
    };
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(document.ratatui_lines()), area);
}

fn render_slash_command_overlay(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    match &prepared.slash_status {
        SlashCommandStatus::Autocompleting {
            matches,
            selected_index,
            ..
        } => {
            if matches.is_empty() {
                return;
            }
            let max_visible = matches.len().min(6);
            let height = to_u16(max_visible);
            let prompt_area = prepared.areas.prompt;
            if prompt_area.y < height {
                return;
            }
            let popup_area = Rect {
                x: prompt_area.x,
                y: prompt_area.y.saturating_sub(height),
                width: prompt_area.width,
                height,
            };

            frame.render_widget(Clear, popup_area);

            let mut lines = Vec::new();
            for (index, spec) in matches.iter().take(max_visible).enumerate() {
                let is_selected = index == *selected_index;

                let cmd_style = if is_selected {
                    backend_style(StyleId::Active)
                } else {
                    backend_style(StyleId::Command)
                };
                let usage_style = if is_selected {
                    backend_style(StyleId::Active)
                } else {
                    backend_style(StyleId::Muted)
                };
                let summary_style = if is_selected {
                    backend_style(StyleId::Active)
                } else {
                    backend_style(StyleId::Plain)
                };

                let mut spans = vec![
                    Span::styled(format!("/{}", spec.name), cmd_style),
                    Span::styled(format!(" {}", spec.usage), usage_style),
                ];
                if !spec.summary.is_empty() {
                    spans.push(Span::styled(
                        format!(" — {}", spec.summary),
                        summary_style,
                    ));
                }

                lines.push(Line::from(spans));
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                popup_area,
            );
        }
        SlashCommandStatus::Matched {
            spec, syntax_valid, ..
        } => {
            let prompt_area = prepared.areas.prompt;
            if prompt_area.y < 1 {
                return;
            }
            let banner_area = Rect {
                x: prompt_area.x,
                y: prompt_area.y.saturating_sub(1),
                width: prompt_area.width,
                height: 1,
            };

            frame.render_widget(Clear, banner_area);

            let mut spans = vec![
                Span::styled("💡 Usage: ", backend_style(StyleId::Active)),
                Span::styled(format!("/{}", spec.name), backend_style(StyleId::Command)),
                Span::styled(format!(" {}", spec.usage), backend_style(StyleId::Plain)),
            ];
            if !spec.summary.is_empty() {
                spans.push(Span::styled(
                    format!(" — {}", spec.summary),
                    backend_style(StyleId::Muted),
                ));
            }
            if !syntax_valid {
                spans.push(Span::styled(
                    "  ✖ Syntax Error: missing or invalid arguments",
                    backend_style(StyleId::SyntaxError),
                ));
            }

            frame.render_widget(
                Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
                banner_area,
            );
        }
        SlashCommandStatus::None => {}
    }
}

/// Renders the session picker as a full-screen modal overlay.
/// Renders the left-hand session picker pane directly into the allocated layout area.
fn render_session_picker_overlay(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    let Some(picker_state) = prepared.picker_state.as_ref() else {
        return;
    };
    let Some(area) = prepared.areas.session_picker_list else {
        return;
    };

    frame.render_widget(Clear, area);

    // Split left pane: search bar on top (1 row), session list fills the rest.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    // Search bar.
    let search_text = format!("> {}", picker_state.query);
    frame.render_widget(
        Paragraph::new(search_text).style(backend_style(StyleId::Active)),
        chunks[0],
    );

    // Session list.
    let filtered = picker_state.filtered_sessions();
    let items: Vec<ListItem> = filtered
        .iter()
        .map(|(s, _)| {
            let display_title = if s.title.is_empty() { &s.id } else { &s.title };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:10} ", &s.id[..s.id.len().min(10)]),
                    backend_style(StyleId::Muted),
                ),
                Span::styled(display_title.to_string(), backend_style(StyleId::Plain)),
            ]))
        })
        .collect();

    let mut list_state = ListState::default();
    let selected_index = picker_state.list_state.selected();
    list_state.select(selected_index);

    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(backend_style(StyleId::Selection))
            .highlight_symbol("> ")
            .block(
                Block::default()
                    .borders(Borders::RIGHT)
                    .border_style(backend_style(StyleId::Muted))
                    .title(" Sessions "),
            ),
        chunks[1],
        &mut list_state,
    );
}

/// Renders the rewind picker as a full-screen modal overlay.
fn render_rewind_picker_overlay(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    let Some(picker_state) = prepared.rewind_picker_state.as_ref() else {
        return;
    };

    let area = frame.area();
    if area.width < 10 || area.height < 4 {
        return;
    }

    let modal_width = (area.width * 8 / 10).max(40).min(area.width);
    let modal_height = (area.height * 3 / 4).max(6).min(area.height);
    let modal = Rect {
        x: area.x + (area.width.saturating_sub(modal_width)) / 2,
        y: area.y + (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };

    frame.render_widget(Clear, modal);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(backend_style(StyleId::Active))
            .title(" Rewind Session — ↑↓ navigate turn/checkpoint, Enter confirm, Esc cancel "),
        modal,
    );

    let inner = Rect {
        x: modal.x + 1,
        y: modal.y + 1,
        width: modal.width.saturating_sub(2),
        height: modal.height.saturating_sub(2),
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let items: Vec<ListItem> = picker_state
        .options
        .iter()
        .map(|opt| ListItem::new(Line::from(Span::styled(opt.label.clone(), backend_style(StyleId::Plain)))))
        .collect();

    let mut list_state = ListState::default();
    list_state.select(picker_state.list_state.selected());

    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(backend_style(StyleId::Selection))
            .highlight_symbol("> "),
        inner,
        &mut list_state,
    );
}

fn context_label(usage: ContextUsage) -> String {
    format!(
        "{}/{}",
        compact_count(usage.estimated_input_tokens),
        compact_count(usage.max_input_tokens)
    )
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn contains(area: Rect, point: Position) -> bool {
    point.x >= area.x
        && point.x < area.x.saturating_add(area.width)
        && point.y >= area.y
        && point.y < area.y.saturating_add(area.height)
}

fn to_u16(value: usize) -> u16 {
    value.min(usize::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{
        app::Application,
        control::is_directional_formatting,
        domain::{
            ActivityState, ActivityStatus, AgentId, AgentState, AgentStatus, DomainEvent,
            ExternalText, InitialState, MessageRole, ModelState, ProviderKind, ProviderState,
            ProviderTransport, TranscriptPayload, TranscriptSnapshotEntry,
        },
    };

    fn application(prompt: &str) -> Application {
        Application::import(InitialState {
            session_id: ExternalText::new("session"),
            thread_title: ExternalText::new("thread"),
            provider: None,
            model: ModelState {
                model: ExternalText::new("model"),
                reasoning_effort: None,
                service_tier: None,
            },
            developer_mode: true,
            response_streaming: false,
            last_ttft_ms: None,
            transcript: Vec::new(),
            prompt: prompt.to_string(),
            prompt_cursor: prompt.len(),
            queued_steering: None,
            agents: Vec::new(),
            active_activity_ids: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn prepared_prompt_hit_test_uses_the_render_layout() {
        let mut application = application("a好b");
        let prepared = prepare(
            &mut application,
            Rect {
                x: 0,
                y: 0,
                width: 8,
                height: 10,
            },
            Instant::now(),
        );
        let area = prepared.areas().prompt_content;
        let point = Position::new(area.x.saturating_add(2), area.y);
        let position = prepared.prompt_position(point).unwrap();
        application.handle_user_command(crate::app::UserCommand::BeginPromptSelection { position });
        assert_eq!(application.prompt().cursor(), 1);
    }

    #[test]
    fn tiny_frame_areas_never_escape_the_container() {
        for height in 0..6 {
            let area = Rect {
                x: 3,
                y: 4,
                width: 2,
                height,
            };
            let areas = allocate_areas(area, 10, 3);
            assert_eq!(
                areas.status.y.saturating_add(areas.status.height),
                area.y.saturating_add(area.height)
            );
        }
    }
    #[test]
    fn large_prompt_frame_projects_only_cursor_visible_rows() {
        let mut text = (0..20_000)
            .map(|index| format!("line {index:05}\n"))
            .collect::<String>();
        text.push_str("tail");
        let mut application = application(&text);
        let prepared = prepare(&mut application, Rect::new(0, 0, 40, 20), Instant::now());

        assert!(text.len() > 64 * 1024);
        assert_eq!(
            prepared.prompt_document.lines().len(),
            usize::from(prepared.areas.prompt_content.height)
        );
        let projection = prepared
            .prompt_document
            .lines()
            .iter()
            .flat_map(|line| line.runs())
            .map(|run| run.text())
            .collect::<String>();
        assert!(!projection.contains("line 00000"));
        assert!(projection.ends_with("tail"));
        assert!(!projection.contains("output truncated"));
        let cursor = prepared
            .prompt_cursor
            .expect("large prompt cursor remains visible");
        let position = prepared
            .prompt_position(Position::new(cursor.x, cursor.y))
            .expect("visible cursor hit tests through the same viewport");
        drop(prepared);

        application.handle_user_command(crate::app::UserCommand::BeginPromptSelection { position });
        assert_eq!(application.prompt().cursor(), text.len());
    }

    #[test]
    fn status_is_explicitly_bounded_to_one_row_at_tiny_widths() {
        let mut application = application("");
        application.apply_domain_event(DomainEvent::ProviderChanged(ProviderState {
            display_name: ExternalText::new("好\u{1b}[31m\u{202e}provider"),
            kind: ProviderKind::HttpsApi,
            transport: ProviderTransport::Https,
        }));
        application.apply_domain_event(DomainEvent::ModelChanged(ModelState {
            model: ExternalText::new("好model\nsecond"),
            reasoning_effort: Some(ExternalText::new("high")),
            service_tier: Some(ExternalText::new("default")),
        }));

        for width in [0, 1, 9] {
            let document = prepare_status(application.session(), width);
            assert_eq!(document.lines().len(), 1);
            assert!(document.lines()[0].width() <= usize::from(width));
            let text = document.lines()[0]
                .runs()
                .iter()
                .map(|run| run.text())
                .collect::<String>();
            assert!(
                text.chars()
                    .all(|character| !character.is_control()
                        && !is_directional_formatting(character))
            );
            if width == 1 {
                assert_eq!(text, "…");
            }
        }
    }

    #[test]
    fn notice_uses_sanitized_cell_width_for_right_aligned_overlay() {
        let mut app = application("");
        app.apply_domain_event(DomainEvent::Failure(
            "error message".to_string(),
        ));
        // Failure is now routed directly into the transcript as a chat error entry.
        assert_eq!(app.into_final_state().transcript.len(), 1);
        let fresh_app = application("");
        assert!(prepare_notice(&fresh_app, 5).is_none());
    }

    #[test]
    fn transcript_tiny_width_transitions_remain_noninteractive_and_bounded() {
        let mut state = application("").into_final_state();
        state.transcript = vec![TranscriptSnapshotEntry {
            sequence: Some(1),
            payload: TranscriptPayload::Message {
                role: MessageRole::Assistant,
                text: ExternalText::new("好abcdef"),
            },
        }];
        let mut application = Application::import(InitialState {
            session_id: state.session_id,
            thread_title: state.thread_title,
            provider: state.provider,
            model: state.model,
            developer_mode: state.developer_mode,
            response_streaming: state.response_streaming,
            last_ttft_ms: state.last_ttft_ms,
            transcript: state.transcript,
            prompt: state.prompt,
            prompt_cursor: state.prompt_cursor,
            queued_steering: state.queued_steering,
            agents: state.agents,
            active_activity_ids: state.active_activity_ids,
        })
        .unwrap();

        let zero = prepare(&mut application, Rect::new(0, 0, 0, 6), Instant::now());
        assert_eq!(zero.transcript_dimensions().0, 0);
        assert!(zero.transcript_scrollbar.is_none());
        assert_eq!(zero.transcript_position(Position::new(0, 0)), None);
        drop(zero);

        let one = prepare(&mut application, Rect::new(0, 0, 1, 6), Instant::now());
        assert_eq!(one.transcript_dimensions().0, 1);
        assert!(one.transcript_scrollbar.is_none());
        assert_eq!(one.transcript_position(Position::new(1, 0)), None);
        drop(one);

        let wider = prepare(&mut application, Rect::new(0, 0, 8, 6), Instant::now());
        assert!(wider.transcript_dimensions().0 > 1);
        assert!(
            wider.transcript.top_line
                <= wider
                    .transcript
                    .total_lines
                    .saturating_sub(wider.transcript.height)
        );
    }

    #[test]
    fn hostile_external_labels_render_without_executable_terminal_text() {
        let hostile = "\u{1b}[31m\u{202e}好\nnext";
        let mut application = Application::import(InitialState {
            session_id: ExternalText::new(hostile),
            thread_title: ExternalText::new(hostile),
            provider: Some(ProviderState {
                display_name: ExternalText::new(hostile),
                kind: ProviderKind::HttpsApi,
                transport: ProviderTransport::Https,
            }),
            model: ModelState {
                model: ExternalText::new(hostile),
                reasoning_effort: Some(ExternalText::new(hostile)),
                service_tier: Some(ExternalText::new(hostile)),
            },
            developer_mode: true,
            response_streaming: false,
            last_ttft_ms: None,
            transcript: vec![TranscriptSnapshotEntry {
                sequence: Some(1),
                payload: TranscriptPayload::Message {
                    role: MessageRole::Assistant,
                    text: ExternalText::new(hostile),
                },
            }],
            prompt: hostile.to_string(),
            prompt_cursor: hostile.len(),
            queued_steering: Some(ExternalText::new(hostile)),
            agents: vec![AgentState {
                id: AgentId(7),
                path: ExternalText::new(hostile),
                status: AgentStatus::Running,
                last_task_message: Some(ExternalText::new(hostile)),
                last_activity_message: Some(ExternalText::new(hostile)),
            }],
            active_activity_ids: Vec::new(),
        })
        .unwrap();
        application.apply_domain_event(DomainEvent::ActivityChanged(ActivityState {
            id: ExternalText::new(hostile),
            description: ExternalText::new(hostile),
            status: ActivityStatus::Running,
            detail: Some(ExternalText::new(hostile)),
        }));
        application.apply_domain_event(DomainEvent::Failure(hostile.to_string()));

        let width = 40;
        let height = 12;
        let prepared = prepare(
            &mut application,
            Rect::new(0, 0, width, height),
            Instant::now(),
        );
        for area in [
            prepared.areas.transcript,
            prepared.areas.transcript_content,
            prepared.areas.activity,
            prepared.areas.prompt,
            prepared.areas.prompt_content,
            prepared.areas.status,
        ] {
            assert!(area.x.saturating_add(area.width) <= width);
            assert!(area.y.saturating_add(area.height) <= height);
        }

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &prepared)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..height {
            for x in 0..width {
                assert!(
                    buffer[(x, y)]
                        .symbol()
                        .chars()
                        .all(|character| !character.is_control()
                            && !is_directional_formatting(character))
                );
            }
        }
    }
}
