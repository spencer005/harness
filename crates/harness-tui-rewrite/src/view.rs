//! Immutable frame preparation, rendering, and hit testing.

use std::time::Instant;

use crossterm::event::MouseEvent;
use ratatui::{
    Frame,
    layout::{Position, Rect},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use crate::{
    app::{Application, NoticeSeverity},
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
    prompt_cursor: Option<Position>,
}

impl PreparedFrame {
    #[cfg(test)]
    /// Returns allocated frame areas.
    pub(crate) fn areas(&self) -> FrameAreas {
        self.areas
    }

    /// Resolves an in-prompt mouse point to a prompt grapheme boundary.
    pub(crate) fn prompt_position(&self, mouse: MouseEvent) -> Option<PromptPosition> {
        contains(self.areas.prompt_content, mouse).then(|| {
            let row = usize::from(mouse.row.saturating_sub(self.areas.prompt_content.y));
            let cell = usize::from(mouse.column.saturating_sub(self.areas.prompt_content.x));
            self.prompt_viewport.position_at(row, cell)
        })
    }

    /// Resolves any mouse point to a prompt boundary clamped to the prompt.
    pub(crate) fn prompt_position_clamped(&self, mouse: MouseEvent) -> Option<PromptPosition> {
        let area = self.areas.prompt_content;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let column = mouse
            .column
            .clamp(area.x, area.x.saturating_add(area.width.saturating_sub(1)));
        let row = mouse
            .row
            .clamp(area.y, area.y.saturating_add(area.height.saturating_sub(1)));
        Some(self.prompt_viewport.position_at(
            usize::from(row.saturating_sub(area.y)),
            usize::from(column.saturating_sub(area.x)),
        ))
    }

    /// Resolves a transcript mouse point to a stable selectable position.
    pub(crate) fn transcript_position(&self, mouse: MouseEvent) -> Option<TranscriptPosition> {
        if !contains(self.areas.transcript_content, mouse) {
            return None;
        }
        let row = usize::from(mouse.row.saturating_sub(self.areas.transcript_content.y));
        let cell = usize::from(mouse.column.saturating_sub(self.areas.transcript_content.x));
        self.transcript.position_at(row, cell)
    }
    /// Returns whether a mouse event is inside the transcript text area.
    pub(crate) fn transcript_contains(&self, mouse: MouseEvent) -> bool {
        contains(self.areas.transcript_content, mouse)
    }

    /// Resolves a mouse point clamped to the transcript viewport.
    pub(crate) fn transcript_position_clamped(
        &self,
        mouse: MouseEvent,
    ) -> Option<TranscriptPosition> {
        let area = self.areas.transcript_content;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let column = mouse
            .column
            .clamp(area.x, area.x.saturating_add(area.width.saturating_sub(1)));
        let row = mouse
            .row
            .clamp(area.y, area.y.saturating_add(area.height.saturating_sub(1)));
        self.transcript.position_at(
            usize::from(row.saturating_sub(area.y)),
            usize::from(column.saturating_sub(area.x)),
        )
    }

    /// Returns transcript selection scrolling at a viewport edge.
    ///
    /// A transcript at terminal row zero uses its first row as the upward scroll
    /// zone because Crossterm cannot report a negative pointer row.
    pub(crate) fn transcript_selection_scroll(
        &self,
        mouse: MouseEvent,
    ) -> Option<(TranscriptScrollDirection, usize)> {
        let area = self.areas.transcript_content;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let cell = usize::from(
            mouse
                .column
                .clamp(area.x, area.x.saturating_add(area.width.saturating_sub(1)))
                .saturating_sub(area.x),
        );
        let after_final_row = area.y.saturating_add(area.height);
        if mouse.row < area.y || (area.y == 0 && mouse.row == 0) {
            Some((TranscriptScrollDirection::Older, cell))
        } else if mouse.row >= after_final_row {
            Some((TranscriptScrollDirection::Newer, cell))
        } else {
            None
        }
    }

    /// Maps a scrollbar press to an absolute top line and stable thumb offset.
    pub(crate) fn transcript_scrollbar_press(&self, mouse: MouseEvent) -> Option<(usize, u16)> {
        let scrollbar = self.transcript_scrollbar?;
        contains(scrollbar.area, mouse)
            .then(|| scrollbar.press(mouse.row, self.transcript.top_line))
    }

    /// Maps any captured drag point to the nearest scrollbar track row.
    pub(crate) fn transcript_scrollbar_top_line_clamped(
        &self,
        mouse: MouseEvent,
        thumb_offset: u16,
    ) -> Option<usize> {
        self.transcript_scrollbar
            .map(|scrollbar| scrollbar.top_line_for_pointer(mouse.row, thumb_offset))
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
    let areas = allocate_areas(area, prompt_metrics.line_count, activity_line_count);
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
    let prompt_cursor = prompt_cursor_position(&prompt_viewport, areas.prompt_content);
    let prompt_document = prepare_prompt(&prompt_viewport, areas.prompt_content.width);
    let status_document = prepare_status(application.session(), areas.status.width);
    let notice_document = prepare_notice(application, area.width);
    let transcript = application.transcript_mut().viewport(
        areas.transcript_content.width,
        usize::from(areas.transcript_content.height),
    );
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
        prompt_cursor,
    }
}

/// Renders one immutable prepared frame.
pub(crate) fn render(frame: &mut Frame<'_>, prepared: &PreparedFrame) {
    render_transcript(frame, prepared);
    render_activity(frame, prepared);
    render_prompt(frame, prepared);
    render_status(frame, prepared);
    render_notice(frame, prepared);
    if let Some(cursor) = prepared.prompt_cursor {
        frame.set_cursor_position(cursor);
    }
}

fn allocate_areas(area: Rect, prompt_line_count: usize, activity_line_count: usize) -> FrameAreas {
    let total_height = usize::from(area.height);
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
        x: area.x,
        y: area.y,
        width: area.width,
        height: to_u16(transcript_height),
    };
    let activity = Rect {
        x: area.x,
        y: transcript.y.saturating_add(transcript.height),
        width: area.width,
        height: to_u16(activity_height),
    };
    let prompt = Rect {
        x: area.x,
        y: activity.y.saturating_add(activity.height),
        width: area.width,
        height: to_u16(prompt_height),
    };
    let status = Rect {
        x: area.x,
        y: prompt.y.saturating_add(prompt.height),
        width: area.width,
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

fn prepare_prompt(prompt_viewport: &PromptViewport, width: u16) -> DisplayDocument<LaidOut> {
    let mut builder = RawDocumentBuilder::new();
    for (line_index, line) in prompt_viewport.lines().iter().enumerate() {
        if line_index > 0 {
            builder.line_break();
        }
        for run in line.runs() {
            builder.plain(
                run.text(),
                if run.selected() {
                    StyleId::Selection
                } else {
                    StyleId::Prompt
                },
                true,
            );
        }
    }
    let (bytes, lines, cells) = prompt_viewport.projection_metrics();
    prepare_document(
        builder.build(),
        DocumentLimits::prompt_viewport(bytes, lines, cells),
        width,
    )
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
    if let Some(notice) = application.notice() {
        builder.plain(
            notice.text.as_str(),
            match notice.severity {
                NoticeSeverity::Information => StyleId::Active,
                NoticeSeverity::Warning => StyleId::Queued,
                NoticeSeverity::Error => StyleId::Error,
            },
            true,
        );
        has_notice = true;
    }
    if application.exit_armed() {
        if has_notice {
            builder.plain(" · ", StyleId::Muted, false);
        }
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

fn contains(area: Rect, mouse: MouseEvent) -> bool {
    mouse.column >= area.x
        && mouse.column < area.x.saturating_add(area.width)
        && mouse.row >= area.y
        && mouse.row < area.y.saturating_add(area.height)
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
        let mouse = MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: area.x.saturating_add(2),
            row: area.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let position = prepared.prompt_position(mouse).unwrap();
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
            .prompt_position(mouse_at(cursor.x, cursor.y))
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
        let mut application = application("");
        application.apply_domain_event(DomainEvent::Failure(
            "好好\nsecond\u{1b}[31m\u{202e}".to_string(),
        ));
        let document = prepare_notice(&application, 5).expect("notice is prepared");
        let line = &document.lines()[0];
        let text = line.runs().iter().map(|run| run.text()).collect::<String>();

        assert_eq!(document.lines().len(), 1);
        assert_eq!(text, "好好…");
        assert_eq!(line.width(), 5);
        assert_eq!(
            notice_area(&document, Rect::new(7, 9, 5, 1)),
            Some(Rect::new(7, 9, 5, 1))
        );
        assert_eq!(
            notice_area(&document, Rect::new(7, 9, 3, 1)),
            Some(Rect::new(7, 9, 3, 1))
        );
        assert_eq!(notice_area(&document, Rect::new(7, 9, 5, 0)), None);
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
        assert_eq!(zero.transcript_position(mouse_at(0, 0)), None);
        drop(zero);

        let one = prepare(&mut application, Rect::new(0, 0, 1, 6), Instant::now());
        assert_eq!(one.transcript_dimensions().0, 1);
        assert!(one.transcript_scrollbar.is_none());
        assert_eq!(one.transcript_position(mouse_at(1, 0)), None);
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

    fn mouse_at(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }
}
