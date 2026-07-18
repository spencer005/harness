//! Terminal user interface for interactive harness sessions.

use std::{
    collections::{HashMap, VecDeque},
    io,
    io::{Stdout, Write},
    ops::Range,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::MoveTo,
    event,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, terminal,
    terminal::{Clear as TerminalClear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
#[cfg(test)]
use harness_core::sessions::FreeformToolOutputRecord;
use harness_core::{
    UiSnapshot, UiTranscriptEntry,
    actors::{ActorHandle, ActorReceiver, RuntimeCommand, RuntimeEvent},
    compact::ContextWindowUsage,
    sessions::{
        FreeformToolCallRecord, FunctionToolCallRecord, InspectReadDisplayRecord, MessageRecord,
        SessionRecordKind, ToolOutputDisplayRecord,
    },
    steering::{SteeringMode, append_queued_steering_text},
    subagents::AgentStatus,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Style as SyntectStyle, Theme, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Prompt input editing primitives used by the TUI.
pub mod input;
/// Transcript storage and visual-line indexing for the TUI.
pub mod transcript;
mod transcript_view;

use crate::{
    input::{InputEditor, VerticalDirection, byte_index_for_char_column, floor_char_boundary},
    transcript::{TranscriptEntryId, TranscriptLineIndex},
    transcript_view::{TranscriptSelection, TranscriptSelectionPoint, TranscriptView},
};

const MIN_INPUT_AREA_HEIGHT: u16 = 3;
const MAX_INPUT_AREA_PERCENT: u16 = 40;
const INPUT_LEFT_MARGIN_WIDTH: u16 = 2;
const INPUT_AREA_BG: Color = Color::Rgb(24, 24, 32);
const INPUT_SELECTION_BG: Color = Color::Rgb(66, 82, 112);
const INPUT_MARGIN_LINE_CHAR: char = '│';
const INPUT_MORE_ABOVE_CHAR: char = '▲';
const INPUT_MORE_BELOW_CHAR: char = '▼';
const CTRL_C_WARNING_TEXT: &str = "press Ctrl-C again to exit";
const CTRL_C_WARNING_FG: Color = Color::Rgb(255, 218, 92);
const YANK_SELECTION_TEXT: &str = "press y to yank selection";
const TOAST_BG: Color = Color::Rgb(36, 32, 24);
const TRANSCRIPT_ASSISTANT_MARKER: &str = "•";
const TRANSCRIPT_INPUT_MARKER: &str = "»";
const TRANSCRIPT_TOOL_MARKER: &str = "⚙";
const TRANSCRIPT_EVENT_MARKER: &str = "·";
const TRANSCRIPT_ERROR_MARKER: &str = "!";
const TRANSCRIPT_ASSISTANT_FG: Color = Color::Rgb(142, 132, 190);
const TRANSCRIPT_DEVELOPER_FG: Color = Color::Rgb(196, 170, 104);
const TRANSCRIPT_USER_FG: Color = Color::Rgb(128, 170, 190);
const TRANSCRIPT_TOOL_FG: Color = Color::Rgb(168, 138, 188);
const TRANSCRIPT_EVENT_FG: Color = Color::Gray;
const TRANSCRIPT_ASSISTANT_TEXT_FG: Color = Color::Rgb(214, 212, 226);
const TRANSCRIPT_INPUT_TEXT_FG: Color = Color::Rgb(226, 220, 206);
const TRANSCRIPT_INLINE_CODE_FG: Color = Color::Rgb(238, 178, 97);
const TRANSCRIPT_FAILED_PATCH_FG: Color = Color::Rgb(132, 126, 138);
const TRANSCRIPT_TOOL_ERROR_FG: Color = Color::Rgb(132, 44, 44);
const DIFF_ADDED_BG: Color = Color::Rgb(33, 58, 43);
const DIFF_REMOVED_BG: Color = Color::Rgb(74, 34, 29);
const DIFF_ADDED_FG: Color = Color::Rgb(118, 205, 148);
const DIFF_REMOVED_FG: Color = Color::Rgb(224, 118, 102);
const ACTIVITY_PRIMARY_FG: Color = Color::Rgb(208, 208, 214);
const ACTIVITY_SECONDARY_FG: Color = Color::DarkGray;
const ACTIVITY_QUEUED_FG: Color = Color::Rgb(196, 170, 104);
const ACTIVITY_AGENT_PATH_FG: Color = Color::Cyan;
const ACTIVITY_AREA_BG: Color = Color::Rgb(20, 21, 28);
const ACTIVITY_GUTTER_FG: Color = Color::Rgb(74, 76, 92);
const STATUS_BG: Color = Color::Rgb(18, 18, 24);
const STATUS_LABEL_FG: Color = Color::DarkGray;
const STATUS_VALUE_FG: Color = Color::Rgb(208, 208, 214);
const STATUS_CONTEXT_WARNING_FG: Color = Color::LightYellow;
const MAX_TRANSCRIPT_ENTRIES: usize = 512;
const MAX_TRANSCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_TRANSCRIPT_ENTRY_BYTES: usize = 64 * 1024;
const TRANSCRIPT_PAGE_LINE_LIMIT: usize = 96;
const TRANSCRIPT_TRUNCATED_MARKER: &str = "… transcript entry truncated …\n";
const TRANSCRIPT_MOUSE_SCROLL_LINES: usize = 3;
const MAX_SYNTAX_HIGHLIGHT_LINE_BYTES: usize = 4 * 1024;
const MAX_SYNTAX_HIGHLIGHT_CACHE_ENTRIES: usize = 4096;
const MAX_TOOL_OUTPUT_DISPLAY_LINES: usize = 10;
const MAX_RUNTIME_EVENT_DRAIN: usize = 256;
const TRANSCRIPT_SCROLLBAR_VISIBLE_DURATION: Duration = Duration::from_secs(5);

/// Mutable state owned by the terminal UI loop.
#[derive(Debug, Clone)]
pub struct TuiApp {
    snapshot: UiSnapshot,
    transcript_view: TranscriptView,
    should_quit: bool,
    ctrl_c_exit_armed: bool,
    agentic_loop_working: bool,
    agentic_loop_started_at: Option<Instant>,
    transcript_scrollbar_visible_until: Option<Instant>,
    context_window_usage: Option<ContextWindowUsage>,
    input_editor: InputEditor,
    last_input_content_area: Rect,
    last_input_scroll: u16,
    /// Live transcript frames keyed by subagent activity id.
    subagent_activity_frames: HashMap<String, SubagentActivityFrame>,
}

/// Action produced by keyboard input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    /// Submit input to the runtime.
    SubmitInput(String),
    /// Queue steering text for the active response.
    QueueSteering(String),
    /// Apply steering immediately to the active response.
    ApplySteering {
        /// Steering text submitted by the user.
        text: String,
        /// Steering delivery mode selected by the user.
        mode: SteeringMode,
    },
    /// Load an older persisted transcript page.
    LoadTranscriptPage {
        /// Sequence number before which older lines are requested.
        before_seq: Option<u64>,
        /// Maximum number of transcript lines to load.
        max_lines: usize,
    },
    /// Copy selected transcript content to the clipboard.
    CopySelection(String),
}

impl TuiApp {
    /// Create an app from a renderable UI snapshot.
    pub fn new(mut snapshot: UiSnapshot) -> Self {
        let transcript_view = TranscriptView::from_snapshot(&mut snapshot);
        let mut app = Self {
            snapshot,
            transcript_view,
            should_quit: false,
            ctrl_c_exit_armed: false,
            agentic_loop_working: false,
            agentic_loop_started_at: None,
            transcript_scrollbar_visible_until: Some(
                Instant::now() + TRANSCRIPT_SCROLLBAR_VISIBLE_DURATION,
            ),
            context_window_usage: None,
            input_editor: InputEditor::default(),
            last_input_content_area: Rect::default(),
            last_input_scroll: 0,
            subagent_activity_frames: HashMap::new(),
        };
        app.transcript_view.enforce_memory_limit();
        app
    }

    /// Return the current render snapshot.
    pub fn snapshot(&self) -> UiSnapshot {
        self.transcript_view.materialize_snapshot(&self.snapshot)
    }

    /// Consume the app and return its final snapshot.
    pub fn into_snapshot(self) -> UiSnapshot {
        self.transcript_view.into_snapshot(self.snapshot)
    }

    /// Return the transcript entry ID at a display-order index.
    pub fn transcript_entry_id_at(&self, index: usize) -> Option<TranscriptEntryId> {
        self.transcript_view.entry_id_at(index)
    }

    /// Build the cached transcript viewport and return its rendered line count.
    pub fn transcript_viewport_line_count(
        &mut self,
        viewport_height: usize,
        viewport_width: u16,
    ) -> usize {
        self.transcript_view
            .viewport_ref(viewport_height, viewport_width, false)
            .lines
            .len()
    }

    /// Update transcript render metrics used by scroll, mouse, and selection handling.
    pub fn update_transcript_render_metrics(
        &mut self,
        viewport_height: usize,
        scroll_offset: usize,
        transcript_area: Rect,
    ) {
        self.transcript_view
            .update_render_metrics(TranscriptRenderMetrics {
                viewport_height,
                scroll_offset,
                transcript_area,
            });
    }

    /// Scroll to the oldest loaded transcript line and request an older page if needed.
    pub fn request_older_transcript_page_at_top(&mut self) -> Option<TuiAction> {
        self.scroll_transcript(|view| view.scroll_to_top());
        self.transcript_view.request_page_if_needed()
    }

    /// Mark the transcript page request as completed without adding page content.
    pub fn clear_transcript_page_loading(&mut self) {
        self.transcript_view.page_loading = false;
    }

    /// Select transcript content between two viewport positions and return the selected text.
    pub fn select_transcript_text(
        &mut self,
        anchor_visual_line: usize,
        anchor_content_col: usize,
        cursor_visual_line: usize,
        cursor_content_col: usize,
    ) -> Option<String> {
        self.transcript_view.selection = Some(TranscriptSelection {
            anchor: TranscriptSelectionPoint {
                visual_line: anchor_visual_line,
                content_col: anchor_content_col,
            },
            cursor: TranscriptSelectionPoint {
                visual_line: cursor_visual_line,
                content_col: cursor_content_col,
            },
        });
        self.transcript_view.selected_text()
    }

    /// Replace one transcript entry in the in-memory transcript view.
    pub fn replace_transcript_entry(
        &mut self,
        entry_id: TranscriptEntryId,
        line: String,
    ) -> Option<()> {
        self.transcript_view.replace_entry_text(entry_id, line)
    }

    /// Insert one transcript entry after an existing entry in the in-memory transcript view.
    pub fn insert_transcript_entry_after(
        &mut self,
        entry_id: TranscriptEntryId,
        line: String,
    ) -> Option<TranscriptEntryId> {
        self.transcript_view.insert_entry_after(entry_id, line)
    }

    /// Truncate all transcript entries after an existing entry.
    pub fn truncate_transcript_after_entry(
        &mut self,
        entry_id: TranscriptEntryId,
    ) -> Option<Vec<TranscriptEntryId>> {
        self.transcript_view.truncate_after_entry(entry_id)
    }

    /// Whether the UI loop should stop.
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Apply a keyboard event to the TUI state and return any runtime action.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<TuiAction> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        if key.kind == KeyEventKind::Repeat && !is_repeatable_input_key(key) {
            return None;
        }

        match key.code {
            KeyCode::Esc => {
                self.ctrl_c_exit_armed = false;
                if let Some(prompt) = self.snapshot.queued_steering_prompt.take() {
                    return Some(TuiAction::ApplySteering {
                        text: prompt,
                        mode: SteeringMode::InterruptNow,
                    });
                }
                if self.snapshot.response_streaming {
                    let text = self.take_input_text();
                    return Some(TuiAction::ApplySteering {
                        text,
                        mode: SteeringMode::InterruptNow,
                    });
                }
            }
            KeyCode::Char(ch)
                if ch.eq_ignore_ascii_case(&'y')
                    && key.modifiers.is_empty()
                    && self.transcript_view.has_selection() =>
            {
                self.ctrl_c_exit_armed = false;
                if let Some(text) = self.selected_transcript_text() {
                    return Some(TuiAction::CopySelection(text));
                }
            }
            KeyCode::Char(ch)
                if ch.eq_ignore_ascii_case(&'c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.ctrl_c_exit_armed = false;
                if let Some(text) = self.selected_transcript_text() {
                    return Some(TuiAction::CopySelection(text));
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.snapshot.input.is_empty() {
                    if self.ctrl_c_exit_armed {
                        self.should_quit = true;
                    } else {
                        self.ctrl_c_exit_armed = true;
                    }
                } else {
                    self.input_editor.clear(&mut self.snapshot);
                    self.ctrl_c_exit_armed = false;
                }
            }
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.undo(&mut self.snapshot);
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.redo(&mut self.snapshot);
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.move_to_line_start(&mut self.snapshot);
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.move_to_line_end(&mut self.snapshot);
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.insert_char(&mut self.snapshot, '\n');
            }
            KeyCode::Char(ch) if is_text_input(key.modifiers) => {
                self.ctrl_c_exit_armed = false;
                self.transcript_view.clear_selection();
                self.input_editor
                    .insert_char(&mut self.snapshot, text_input_char(ch, key.modifiers));
            }
            KeyCode::Left
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.ctrl_c_exit_armed = false;
                self.transcript_view.clear_selection();
                self.input_editor
                    .select_to_previous_word(&mut self.snapshot);
            }
            KeyCode::Right
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.ctrl_c_exit_armed = false;
                self.transcript_view.clear_selection();
                self.input_editor.select_to_next_word(&mut self.snapshot);
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.move_to_previous_word(&mut self.snapshot);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.move_to_next_word(&mut self.snapshot);
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.ctrl_c_exit_armed = false;
                self.transcript_view.clear_selection();
                self.input_editor
                    .select_to_previous_char(&mut self.snapshot);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.ctrl_c_exit_armed = false;
                self.transcript_view.clear_selection();
                self.input_editor.select_to_next_char(&mut self.snapshot);
            }
            KeyCode::Left => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.move_to_previous_char(&mut self.snapshot);
            }
            KeyCode::Right => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.move_to_next_char(&mut self.snapshot);
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.ctrl_c_exit_armed = false;
                self.transcript_view.clear_selection();
                self.input_editor
                    .select_vertically(&mut self.snapshot, VerticalDirection::Up);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.ctrl_c_exit_armed = false;
                self.transcript_view.clear_selection();
                self.input_editor
                    .select_vertically(&mut self.snapshot, VerticalDirection::Down);
            }
            KeyCode::Up => {
                self.ctrl_c_exit_armed = false;
                self.input_editor
                    .move_vertically(&mut self.snapshot, VerticalDirection::Up);
            }
            KeyCode::Down => {
                self.ctrl_c_exit_armed = false;
                self.input_editor
                    .move_vertically(&mut self.snapshot, VerticalDirection::Down);
            }
            KeyCode::PageUp => {
                self.ctrl_c_exit_armed = false;
                self.scroll_transcript(|view| view.scroll_up_page());
                return self.transcript_view.request_page_if_needed();
            }
            KeyCode::PageDown => {
                self.ctrl_c_exit_armed = false;
                self.scroll_transcript(|view| view.scroll_down_page());
            }
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ctrl_c_exit_armed = false;
                self.scroll_transcript(|view| view.scroll_to_top());
                return self.transcript_view.request_page_if_needed();
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ctrl_c_exit_armed = false;
                self.scroll_transcript(|view| view.scroll_to_bottom());
            }
            KeyCode::Home => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.move_to_line_start(&mut self.snapshot);
            }
            KeyCode::End => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.move_to_line_end(&mut self.snapshot);
            }
            KeyCode::Backspace
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.ctrl_c_exit_armed = false;
                self.input_editor
                    .delete_word_before_cursor(&mut self.snapshot);
            }
            KeyCode::Backspace => {
                self.ctrl_c_exit_armed = false;
                self.input_editor
                    .delete_char_before_cursor(&mut self.snapshot);
            }
            KeyCode::Delete => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.delete_char_at_cursor(&mut self.snapshot);
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                self.ctrl_c_exit_armed = false;
                if let Some(input) = self.input_editor.submit(&mut self.snapshot) {
                    if self.snapshot.response_streaming && !is_slash_command(&input) {
                        return Some(TuiAction::QueueSteering(input));
                    }
                    return Some(TuiAction::SubmitInput(input));
                }
            }
            KeyCode::Enter => {
                self.ctrl_c_exit_armed = false;
                self.input_editor.insert_char(&mut self.snapshot, '\n');
            }
            _ => {}
        }
        None
    }

    /// Insert bracketed paste content as literal input text.
    pub fn handle_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.ctrl_c_exit_armed = false;
        self.transcript_view.clear_selection();
        self.input_editor
            .insert_paste_text(&mut self.snapshot, text);
    }

    /// Apply mouse input to the TUI state.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<TuiAction> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.ctrl_c_exit_armed = false;
                if let Some(cursor) = self.input_cursor_for_mouse(mouse) {
                    self.transcript_view.clear_selection();
                    self.input_editor
                        .begin_mouse_selection(&mut self.snapshot, cursor);
                    return None;
                }
                self.input_editor.clear_selection();
                self.transcript_view.begin_mouse_selection(mouse);
                None
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.ctrl_c_exit_armed = false;
                if self.input_editor.mouse_selection_active() {
                    let cursor = self.input_cursor_for_mouse_clamped(mouse);
                    self.input_editor
                        .drag_mouse_selection(&mut self.snapshot, cursor);
                    return None;
                }
                let scroll_offset = self.transcript_view.scroll_offset;
                let action = self.transcript_view.drag_mouse_selection(mouse);
                if self.transcript_view.scroll_offset != scroll_offset {
                    self.reveal_transcript_scrollbar();
                }
                action
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.ctrl_c_exit_armed = false;
                if self.input_editor.mouse_selection_active() {
                    let cursor = self.input_cursor_for_mouse_clamped(mouse);
                    self.input_editor
                        .finish_mouse_selection(&mut self.snapshot, cursor);
                    return None;
                }
                self.transcript_view.finish_mouse_selection(mouse);
                None
            }
            MouseEventKind::ScrollUp => {
                self.ctrl_c_exit_armed = false;
                self.scroll_transcript(|view| view.scroll_up_by(TRANSCRIPT_MOUSE_SCROLL_LINES));
                self.transcript_view.request_page_if_needed()
            }
            MouseEventKind::ScrollDown => {
                self.ctrl_c_exit_armed = false;
                self.scroll_transcript(|view| view.scroll_down_by(TRANSCRIPT_MOUSE_SCROLL_LINES));
                None
            }
            _ => None,
        }
    }

    fn take_input_text(&mut self) -> String {
        self.input_editor.take_trimmed_text(&mut self.snapshot)
    }

    fn queue_steering_text(&mut self, text: String) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.snapshot.queued_steering_prompt = Some(append_queued_steering_text(
            self.snapshot.queued_steering_prompt.as_deref(),
            text,
        ));
    }

    fn apply_queued_steering_state(&mut self, prompt: Option<String>) {
        let Some(prompt) = prompt else {
            self.snapshot.queued_steering_prompt = None;
            return;
        };
        if let Some(local_prompt) = self.snapshot.queued_steering_prompt.as_deref()
            && complete_prompt_prefix(local_prompt, &prompt)
        {
            return;
        }
        self.snapshot.queued_steering_prompt = Some(prompt);
    }

    fn update_input_render_metrics(&mut self, content_area: Rect, scroll_offset: u16) {
        self.last_input_content_area = content_area;
        self.last_input_scroll = scroll_offset;
    }

    fn input_cursor_for_mouse(&self, mouse: MouseEvent) -> Option<usize> {
        if !mouse_in_rect(mouse, self.last_input_content_area) {
            return None;
        }
        let local_col = mouse.column.saturating_sub(self.last_input_content_area.x);
        let local_row = mouse.row.saturating_sub(self.last_input_content_area.y);
        Some(input_byte_index_for_visual_position(
            &self.snapshot.input,
            self.last_input_content_area.width,
            local_row.saturating_add(self.last_input_scroll),
            local_col,
        ))
    }

    fn input_cursor_for_mouse_clamped(&self, mouse: MouseEvent) -> usize {
        let area = self.last_input_content_area;
        if area.width == 0 || area.height == 0 {
            return self.snapshot.input_cursor;
        }
        let column = mouse
            .column
            .clamp(area.x, area.x.saturating_add(area.width.saturating_sub(1)));
        let row = mouse
            .row
            .clamp(area.y, area.y.saturating_add(area.height.saturating_sub(1)));
        input_byte_index_for_visual_position(
            &self.snapshot.input,
            area.width,
            row.saturating_sub(area.y)
                .saturating_add(self.last_input_scroll),
            column.saturating_sub(area.x),
        )
    }

    /// Apply a runtime event to the render snapshot.
    pub fn apply_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::TranscriptPage(page) => {
                self.transcript_view.apply_page(page);
            }
            RuntimeEvent::TranscriptLine(line) => {
                self.transcript_view
                    .append_entry(UiTranscriptEntry::Text(line));
            }
            RuntimeEvent::FreeformToolCall(call) => {
                self.transcript_view
                    .append_entry(UiTranscriptEntry::SessionRecord(
                        SessionRecordKind::FreeformToolCall(call),
                    ));
            }
            RuntimeEvent::FreeformToolOutput(output) => {
                self.transcript_view
                    .append_entry(UiTranscriptEntry::SessionRecord(
                        SessionRecordKind::FreeformToolOutput(output),
                    ));
            }
            RuntimeEvent::FunctionToolCall(call) => {
                self.transcript_view
                    .append_entry(UiTranscriptEntry::SessionRecord(
                        SessionRecordKind::FunctionToolCall(call),
                    ));
            }
            RuntimeEvent::FunctionToolOutput(output) => {
                self.transcript_view
                    .append_entry(UiTranscriptEntry::SessionRecord(
                        SessionRecordKind::FunctionToolOutput(output),
                    ));
            }
            RuntimeEvent::ModelSettingsChanged(settings) => {
                self.snapshot.thread_title = format!("new_harness · {}", settings.model);
                self.snapshot.model_settings = settings;
            }
            RuntimeEvent::ProviderChanged(provider) => {
                self.snapshot.provider = Some(provider);
            }
            RuntimeEvent::ContextWindowUsage(usage) => {
                self.context_window_usage = Some(usage);
            }
            RuntimeEvent::AgenticLoopStarted => {
                if !self.agentic_loop_working {
                    self.agentic_loop_working = true;
                    self.agentic_loop_started_at = Some(Instant::now());
                }
            }
            RuntimeEvent::AgenticLoopCompleted => {
                self.agentic_loop_working = false;
                self.agentic_loop_started_at = None;
            }
            RuntimeEvent::DeveloperModeChanged(enabled) => {
                self.snapshot.developer_mode = enabled;
            }
            RuntimeEvent::ResponseStreamStarted => {
                self.snapshot.response_streaming = true;
                self.snapshot.last_ttft_ms = None;
                self.transcript_view.begin_response_stream();
            }
            RuntimeEvent::AssistantFirstToken { ttft_ms } => {
                self.snapshot.last_ttft_ms = Some(ttft_ms);
            }
            RuntimeEvent::AssistantTextDelta(delta) => {
                self.transcript_view.append_assistant_delta(&delta);
            }
            RuntimeEvent::ResponseStreamCompleted => {
                self.snapshot.response_streaming = false;
                self.transcript_view.complete_response_stream();
            }
            RuntimeEvent::Responses(_) => {}
            RuntimeEvent::AgentUpdated(summary) => {
                self.apply_agent_updated(summary);
            }
            RuntimeEvent::AgentRemoved(agent_id) => {
                if let Some(agent) = self.snapshot.agents.iter().find(|a| a.id == agent_id) {
                    self.render_subagent_activity_frame(
                        format!("agent-{}", agent_id.0),
                        format!("Agent {}", agent.path),
                        "removed".to_string(),
                        "removed".to_string(),
                    );
                }
                self.snapshot.agents.retain(|agent| agent.id != agent_id);
            }
            RuntimeEvent::CompactCompleted(result) => {
                self.transcript_view
                    .append_line(format!("compact: {}", result.summary));
            }
            RuntimeEvent::SteeringQueued(prompt) => {
                self.apply_queued_steering_state(prompt);
            }
            RuntimeEvent::AgentMailboxUpdate { agent_id } => {
                self.transcript_view
                    .append_line(format!("agent mailbox update: {}", agent_id.0));
            }
            RuntimeEvent::SubagentActivity {
                activity_id,
                description,
                status,
                detail,
            } => {
                self.apply_subagent_activity(activity_id, description, status, detail);
            }
            RuntimeEvent::ShutdownComplete => {
                self.should_quit = true;
            }
        }
    }

    fn selected_transcript_text(&mut self) -> Option<String> {
        self.transcript_view.selected_text()
    }

    fn working_elapsed(&self) -> Option<Duration> {
        if self.agentic_loop_working {
            self.agentic_loop_started_at
                .map(|started| started.elapsed())
        } else {
            None
        }
    }

    fn apply_subagent_activity(
        &mut self,
        activity_id: String,
        description: String,
        status: String,
        detail: Option<String>,
    ) {
        match status.as_str() {
            "running" => {
                if !self.snapshot.active_activities.contains(&activity_id) {
                    self.snapshot.active_activities.push(activity_id.clone());
                }
                self.render_subagent_activity_frame(
                    activity_id,
                    description,
                    "running".to_string(),
                    detail.unwrap_or_else(|| "running".to_string()),
                );
            }
            "completed" => {
                self.snapshot
                    .active_activities
                    .retain(|id| id != &activity_id);
                self.render_subagent_activity_frame(
                    activity_id,
                    description,
                    "completed".to_string(),
                    detail.unwrap_or_else(|| "completed".to_string()),
                );
            }
            "failed" => {
                self.snapshot
                    .active_activities
                    .retain(|id| id != &activity_id);
                self.render_subagent_activity_frame(
                    activity_id,
                    description,
                    "failed".to_string(),
                    detail.unwrap_or_else(|| "failed".to_string()),
                );
            }
            _ => {}
        }
    }
    fn render_subagent_activity_frame(
        &mut self,
        activity_id: String,
        title: String,
        status: String,
        detail: String,
    ) {
        let frame = self
            .subagent_activity_frames
            .entry(activity_id)
            .or_insert_with(|| SubagentActivityFrame {
                entry_id: None,
                title: title.clone(),
                status: String::new(),
                detail: String::new(),
            });
        frame.title = title;
        frame.status = status;
        frame.detail = detail;
        let content = subagent_activity_frame_content(frame);
        if let Some(entry_id) = frame.entry_id {
            self.transcript_view.replace_entry_text(entry_id, content);
        } else {
            frame.entry_id = Some(self.transcript_view.store.push_line(content));
        }
    }

    fn apply_agent_updated(&mut self, summary: harness_core::subagents::AgentSummary) {
        let old_status = self
            .snapshot
            .agents
            .iter()
            .find(|agent| agent.id == summary.id)
            .map(|agent| agent.status.clone());

        if old_status.as_ref() != Some(&summary.status) {
            let path = summary.path.clone();
            let (status, detail) = match &summary.status {
                AgentStatus::Running => {
                    let task_msg = summary.last_task_message.as_deref().unwrap_or("");
                    if task_msg.is_empty() {
                        ("running".to_string(), "running".to_string())
                    } else {
                        ("running".to_string(), task_msg.to_string())
                    }
                }
                AgentStatus::Waiting => ("waiting".to_string(), "waiting".to_string()),
                AgentStatus::Completed(message) => ("completed".to_string(), message.clone()),
                AgentStatus::Failed(message) => ("failed".to_string(), message.clone()),
                AgentStatus::Interrupted => ("interrupted".to_string(), "interrupted".to_string()),
            };
            self.render_subagent_activity_frame(
                format!("agent-{}", summary.id.0),
                format!("Agent {path}"),
                status,
                detail,
            );
        }

        if let Some(existing) = self
            .snapshot
            .agents
            .iter_mut()
            .find(|agent| agent.id == summary.id)
        {
            *existing = summary;
        } else {
            self.snapshot.agents.push(summary);
            self.snapshot
                .agents
                .sort_by(|left, right| left.path.cmp(&right.path));
        }
    }

    fn scroll_transcript(&mut self, scroll: impl FnOnce(&mut TranscriptView)) {
        scroll(&mut self.transcript_view);
        self.reveal_transcript_scrollbar();
    }

    fn reveal_transcript_scrollbar(&mut self) {
        self.reveal_transcript_scrollbar_at(Instant::now());
    }

    fn reveal_transcript_scrollbar_at(&mut self, now: Instant) {
        self.transcript_scrollbar_visible_until = Some(now + TRANSCRIPT_SCROLLBAR_VISIBLE_DURATION);
    }

    fn hide_transcript_scrollbar(&mut self) {
        self.transcript_scrollbar_visible_until = None;
    }

    fn transcript_scrollbar_visible(&self) -> bool {
        self.transcript_scrollbar_visible_at(Instant::now())
    }

    fn transcript_scrollbar_visible_at(&self, now: Instant) -> bool {
        self.transcript_scrollbar_visible_until
            .is_some_and(|expires_at| now < expires_at)
    }

    fn transcript_scrollbar_expires_after(&self) -> Option<Duration> {
        self.transcript_scrollbar_visible_until?
            .checked_duration_since(Instant::now())
    }
}

/// Run the terminal UI until the user exits.
///
/// This owns raw-mode and alternate-screen setup. Confirmed `Ctrl-C` exits;
/// while streaming, `Esc` applies immediate steering.
/// Press `Enter` to append the current input to the local transcript.
pub fn run(snapshot: UiSnapshot) -> io::Result<UiSnapshot> {
    let mut session = TerminalSession::enter()?;
    let mut app = TuiApp::new(snapshot);

    loop {
        session.terminal.draw(|frame| render_app(frame, &mut app))?;
        if app.should_quit() {
            break;
        }
        if let Some(timeout) = app.transcript_scrollbar_expires_after()
            && !event::poll(timeout)?
        {
            app.hide_transcript_scrollbar();
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                if let Some(action) = app.handle_key(key) {
                    apply_local_action(&mut app, session.terminal.backend_mut(), action)?;
                }
            }
            Event::Paste(text) => app.handle_paste(&text),
            Event::Mouse(mouse) => {
                let _ = app.handle_mouse(mouse);
            }
            _ => {}
        }
    }

    Ok(app.into_snapshot())
}

/// Run the terminal UI connected to a harness runtime.
pub async fn run_with_runtime(
    snapshot: UiSnapshot,
    commands: ActorHandle<RuntimeCommand>,
    events: ActorReceiver<RuntimeEvent>,
) -> io::Result<UiSnapshot> {
    let mut session = TerminalSession::enter()?;
    let mut app = TuiApp::new(snapshot);
    let mut terminal_events = EventStream::new();

    loop {
        session.terminal.draw(|frame| render_app(frame, &mut app))?;
        if app.should_quit() {
            let _ = commands.try_send(RuntimeCommand::Shutdown);
            break;
        }

        let scrollbar_timeout = app.transcript_scrollbar_expires_after();
        tokio::select! {
            biased;
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) => {
                        if let Some(action) = app.handle_key(key) {
                            send_runtime_action(
                                &commands,
                                session.terminal.backend_mut(),
                                action.clone(),
                            )?;
                            apply_runtime_action_preview(&mut app, &action);
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        app.handle_paste(&text);
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        if let Some(TuiAction::LoadTranscriptPage { before_seq, max_lines }) =
                            app.handle_mouse(mouse)
                        {
                            send_runtime_command(
                                &commands,
                                RuntimeCommand::LoadTranscriptPage {
                                    before_seq,
                                    max_lines,
                                },
                            )?;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(err),
                    None => break,
                }
            }
            _ = tokio::time::sleep(scrollbar_timeout.unwrap_or(Duration::ZERO)), if scrollbar_timeout.is_some() => {
                app.hide_transcript_scrollbar();
            }
            runtime_event = events.recv() => {
                match runtime_event {
                    Ok(event) => apply_runtime_event_batch(&mut app, event, &events),
                    Err(_) => app.apply_runtime_event(RuntimeEvent::ShutdownComplete),
                }
            }
        }
    }

    Ok(app.into_snapshot())
}

fn apply_runtime_event_batch(
    app: &mut TuiApp,
    first_event: RuntimeEvent,
    events: &ActorReceiver<RuntimeEvent>,
) {
    let mut pending_assistant_delta = None;
    apply_runtime_event_coalesced(app, first_event, &mut pending_assistant_delta);
    for _ in 0..MAX_RUNTIME_EVENT_DRAIN {
        if app.should_quit() {
            break;
        }
        let Ok(event) = events.try_recv() else {
            break;
        };
        apply_runtime_event_coalesced(app, event, &mut pending_assistant_delta);
    }
    flush_assistant_delta(app, &mut pending_assistant_delta);
}

fn apply_runtime_event_coalesced(
    app: &mut TuiApp,
    event: RuntimeEvent,
    pending_assistant_delta: &mut Option<String>,
) {
    match event {
        RuntimeEvent::AssistantTextDelta(delta) => {
            pending_assistant_delta
                .get_or_insert_with(String::new)
                .push_str(&delta);
        }
        event => {
            flush_assistant_delta(app, pending_assistant_delta);
            app.apply_runtime_event(event);
        }
    }
}

fn flush_assistant_delta(app: &mut TuiApp, pending_assistant_delta: &mut Option<String>) {
    if let Some(delta) = pending_assistant_delta.take() {
        app.apply_runtime_event(RuntimeEvent::AssistantTextDelta(delta));
    }
}

fn apply_local_action(
    app: &mut TuiApp,
    clipboard: &mut impl Write,
    action: TuiAction,
) -> io::Result<()> {
    match action {
        TuiAction::SubmitInput(input) => {
            app.apply_runtime_event(RuntimeEvent::TranscriptLine(format!("> {input}")));
        }
        TuiAction::QueueSteering(text) => {
            app.queue_steering_text(text);
        }
        TuiAction::ApplySteering { text, mode: _ } => {
            let transcript = local_steering_transcript(&text);
            app.apply_runtime_event(RuntimeEvent::TranscriptLine(transcript));
            app.apply_runtime_event(RuntimeEvent::SteeringQueued(None));
        }
        TuiAction::LoadTranscriptPage { .. } => {}
        TuiAction::CopySelection(text) => {
            copy_to_clipboard_osc52(clipboard, &text)?;
        }
    }
    Ok(())
}

fn local_steering_transcript(text: &str) -> String {
    if text.trim().is_empty() {
        "interrupt requested".to_string()
    } else {
        format!("steering: {}", text.trim())
    }
}

fn is_slash_command(input: &str) -> bool {
    input.trim_start().starts_with('/')
}

fn apply_runtime_action_preview(app: &mut TuiApp, action: &TuiAction) {
    match action {
        TuiAction::QueueSteering(text) => {
            app.queue_steering_text(text.clone());
        }
        TuiAction::ApplySteering { .. } => {
            app.apply_runtime_event(RuntimeEvent::SteeringQueued(None));
        }
        TuiAction::SubmitInput(_)
        | TuiAction::LoadTranscriptPage { .. }
        | TuiAction::CopySelection(_) => {}
    }
}

fn complete_prompt_prefix(text: &str, prefix: &str) -> bool {
    text == prefix
        || text
            .strip_prefix(prefix)
            .is_some_and(|remaining| remaining.starts_with('\n'))
}

fn send_runtime_action(
    commands: &ActorHandle<RuntimeCommand>,
    clipboard: &mut impl Write,
    action: TuiAction,
) -> io::Result<()> {
    match action {
        TuiAction::SubmitInput(text) => {
            send_runtime_command(commands, RuntimeCommand::SubmitInput { text })?;
        }
        TuiAction::QueueSteering(text) => {
            send_runtime_command(commands, RuntimeCommand::QueueSteering { text })?;
        }
        TuiAction::ApplySteering { text, mode } => {
            send_runtime_command(commands, RuntimeCommand::ApplySteering { text, mode })?;
        }
        TuiAction::LoadTranscriptPage {
            before_seq,
            max_lines,
        } => {
            send_runtime_command(
                commands,
                RuntimeCommand::LoadTranscriptPage {
                    before_seq,
                    max_lines,
                },
            )?;
        }
        TuiAction::CopySelection(text) => {
            copy_to_clipboard_osc52(clipboard, &text)?;
        }
    }
    Ok(())
}

fn send_runtime_command(
    commands: &ActorHandle<RuntimeCommand>,
    command: RuntimeCommand,
) -> io::Result<()> {
    commands
        .try_send(command)
        .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err))
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(
            stdout,
            EnterAlternateScreen,
            TerminalClear(ClearType::All),
            MoveTo(0, 0),
            EnableBracketedPaste,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(
                // Do not request REPORT_ALL_KEYS_AS_ESCAPE_CODES for printable
                // keys: shifted punctuation depends on the user's keyboard
                // layout and must arrive as terminal text, not as guessed
                // base-key + Shift pairs.
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        ) {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(
                stdout,
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            return Err(err);
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = terminal::disable_raw_mode();
                let mut stdout = io::stdout();
                let _ = execute!(
                    stdout,
                    PopKeyboardEnhancementFlags,
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    LeaveAlternateScreen
                );
                return Err(err);
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

fn copy_to_clipboard_osc52(writer: &mut impl Write, text: &str) -> io::Result<()> {
    write!(writer, "\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))?;
    writer.flush()
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        let value = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        encoded.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        encoded.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        if chunk.len() >= 2 {
            encoded.push(ALPHABET[((value >> 6) & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() == 3 {
            encoded.push(ALPHABET[(value & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn is_text_input(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty() || modifiers == KeyModifiers::SHIFT
}

fn text_input_char(ch: char, modifiers: KeyModifiers) -> char {
    // Some terminals report Shift separately for ASCII letters. Punctuation
    // must not be synthesized here because shifted symbols are layout-specific.
    if modifiers == KeyModifiers::SHIFT && ch.is_ascii_lowercase() {
        ch.to_ascii_uppercase()
    } else {
        ch
    }
}

fn is_repeatable_input_key(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(_) => is_text_input(key.modifiers),
        KeyCode::Enter if key.modifiers == KeyModifiers::SHIFT => true,
        KeyCode::Left
        | KeyCode::Right
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::Backspace
        | KeyCode::Delete => true,
        _ => false,
    }
}

#[cfg(test)]
fn ttft_label(snapshot: &UiSnapshot) -> String {
    match (snapshot.response_streaming, snapshot.last_ttft_ms) {
        (true, None) => "pending".to_string(),
        (_, Some(ms)) => format!("{ms}ms"),
        (false, None) => "—".to_string(),
    }
}

#[cfg(test)]
fn status_line(snapshot: &UiSnapshot) -> Line<'static> {
    status_line_with_context(snapshot, None)
}

fn status_line_with_context(
    snapshot: &UiSnapshot,
    context_usage: Option<ContextWindowUsage>,
) -> Line<'static> {
    let session_id = if snapshot.session_id.is_empty() {
        "—"
    } else {
        snapshot.session_id.as_str()
    };

    let provider = snapshot
        .provider
        .as_ref()
        .map(|provider| provider.compact_label())
        .unwrap_or_else(|| "—".to_string());

    Line::from(vec![
        status_label("session"),
        status_value(session_id),
        status_separator(),
        status_label("provider"),
        status_value(provider),
        status_separator(),
        status_label("model"),
        status_value(snapshot.model_settings.model.clone()),
        Span::styled(" · ", Style::default().fg(STATUS_LABEL_FG)),
        status_value(snapshot.model_settings.display_reasoning_effort()),
        Span::styled(" · ", Style::default().fg(STATUS_LABEL_FG)),
        status_value(snapshot.model_settings.display_service_tier()),
        status_separator(),
        status_label("ctx"),
        status_context_value(context_usage),
    ])
}

fn status_context_value(context_usage: Option<ContextWindowUsage>) -> Span<'static> {
    let Some(usage) = context_usage else {
        return status_value("—");
    };
    let style = if usage.estimated_input_tokens >= usage.compact_at_tokens || usage.exceeds_window()
    {
        Style::default().fg(STATUS_CONTEXT_WARNING_FG)
    } else {
        Style::default().fg(STATUS_VALUE_FG)
    };
    Span::styled(
        format!(
            "{}/{}",
            compact_token_count(usage.estimated_input_tokens),
            compact_token_count(usage.max_input_tokens)
        ),
        style,
    )
}

fn compact_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn toast_line(
    show_exit_warning: bool,
    transcript_selection: Option<&TranscriptSelection>,
) -> Option<Line<'static>> {
    let has_selection = transcript_selection.is_some();
    if !show_exit_warning && !has_selection {
        return None;
    }

    let mut spans = vec![Span::styled(" ", toast_style())];
    if has_selection {
        spans.push(Span::styled(
            YANK_SELECTION_TEXT,
            toast_style().fg(STATUS_VALUE_FG),
        ));
    }
    if show_exit_warning {
        if has_selection {
            spans.push(Span::styled("  ·  ", toast_style().fg(STATUS_LABEL_FG)));
        }
        spans.push(Span::styled(
            CTRL_C_WARNING_TEXT,
            toast_style().fg(CTRL_C_WARNING_FG),
        ));
    }
    spans.push(Span::styled(" ", toast_style()));
    Some(Line::from(spans))
}

fn toast_style() -> Style {
    Style::default().bg(TOAST_BG)
}

fn toast_area(container: Rect, line: &Line<'_>) -> Option<Rect> {
    if container.width == 0 || container.height == 0 {
        return None;
    }
    let width = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>()
        .min(usize::from(container.width)) as u16;
    Some(Rect {
        x: container.x + container.width.saturating_sub(width),
        y: container.y,
        width,
        height: 1,
    })
}

fn status_label(label: &'static str) -> Span<'static> {
    Span::styled(format!("{label}: "), Style::default().fg(STATUS_LABEL_FG))
}

fn status_value(value: impl Into<String>) -> Span<'static> {
    single_ansi_text_span(value.into(), Style::default().fg(STATUS_VALUE_FG))
}

fn status_separator() -> Span<'static> {
    Span::styled("  │  ", Style::default().fg(STATUS_LABEL_FG))
}

fn status_area_style() -> Style {
    Style::default().bg(STATUS_BG)
}

fn input_area_style() -> Style {
    Style::default().bg(INPUT_AREA_BG)
}

fn input_margin_style() -> Style {
    Style::default().fg(Color::Gray).bg(INPUT_AREA_BG)
}

fn input_margin_area(area: Rect) -> Rect {
    Rect {
        width: area.width.min(INPUT_LEFT_MARGIN_WIDTH),
        ..area
    }
}

fn input_content_area(area: Rect) -> Rect {
    let margin_width = area.width.min(INPUT_LEFT_MARGIN_WIDTH);
    Rect {
        x: area.x.saturating_add(margin_width),
        width: area.width.saturating_sub(margin_width),
        ..area
    }
}

fn input_margin_lines(
    snapshot: &UiSnapshot,
    area: Rect,
    scroll_offset: u16,
    content_width: u16,
) -> Vec<Line<'static>> {
    let input_lines = input_visual_line_count(&snapshot.input, content_width);
    let more_above = scroll_offset > 0;
    let more_below = input_lines > scroll_offset.saturating_add(area.height);
    let last_row = area.height.saturating_sub(1);

    (0..area.height)
        .map(|row| {
            let guide = if row == 0 && more_above {
                INPUT_MORE_ABOVE_CHAR
            } else if row == last_row && more_below {
                INPUT_MORE_BELOW_CHAR
            } else {
                INPUT_MARGIN_LINE_CHAR
            };
            let text = match area.width {
                0 => String::new(),
                1 => guide.to_string(),
                _ => format!("{guide} "),
            };
            Line::from(Span::styled(text, input_margin_style()))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputCursorMetrics {
    row: u16,
    col: u16,
}

fn input_cursor_metrics(snapshot: &UiSnapshot, wrap_width: u16) -> InputCursorMetrics {
    let cursor = floor_char_boundary(&snapshot.input, snapshot.input_cursor);
    let before_cursor = &snapshot.input[..cursor];
    let wrap_width = wrapped_input_width(wrap_width);
    let mut logical_lines = before_cursor.split('\n').collect::<Vec<_>>();
    let current_line = logical_lines.pop().unwrap_or("");
    let previous_rows = logical_lines
        .iter()
        .map(|line| wrapped_line_count(line, wrap_width))
        .sum::<usize>();
    let current_col = current_line.chars().count();
    let (current_row, col) = cursor_visual_row_col(current_col, wrap_width);
    let row = previous_rows
        .saturating_add(current_row)
        .min(u16::MAX as usize) as u16;
    let col = col.min(u16::MAX as usize) as u16;
    InputCursorMetrics { row, col }
}

fn input_scroll_offset(snapshot: &UiSnapshot, area: Rect) -> u16 {
    if area.height == 0 {
        return 0;
    }
    let metrics = input_cursor_metrics(snapshot, area.width);
    metrics.row.saturating_add(1).saturating_sub(area.height)
}

fn input_area_height(frame_area: Rect, snapshot: &UiSnapshot) -> u16 {
    let max_height = ((frame_area.height as u32 * MAX_INPUT_AREA_PERCENT as u32) / 100)
        .try_into()
        .unwrap_or(u16::MAX);
    input_visual_line_count(&snapshot.input, input_content_width(frame_area.width))
        .max(MIN_INPUT_AREA_HEIGHT)
        .min(max_height.max(MIN_INPUT_AREA_HEIGHT))
}

fn input_content_width(area_width: u16) -> u16 {
    area_width.saturating_sub(area_width.min(INPUT_LEFT_MARGIN_WIDTH))
}

fn wrapped_input_width(width: u16) -> usize {
    usize::from(width.max(1))
}

fn input_visual_line_count(input: &str, wrap_width: u16) -> u16 {
    let wrap_width = wrapped_input_width(wrap_width);
    input
        .split('\n')
        .map(|line| wrapped_line_count(line, wrap_width))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

#[cfg(test)]
fn input_visual_lines(input: &str, wrap_width: u16) -> Vec<Line<'static>> {
    input_visual_lines_with_selection(input, wrap_width, None)
}

fn input_visual_lines_with_selection(
    input: &str,
    wrap_width: u16,
    selection: Option<Range<usize>>,
) -> Vec<Line<'static>> {
    let wrap_width = wrapped_input_width(wrap_width);
    let mut lines = Vec::new();
    let mut offset = 0usize;
    let mut saw_line = false;
    for segment in input.split_inclusive('\n') {
        saw_line = true;
        let logical_line = segment.strip_suffix('\n').unwrap_or(segment);
        push_hard_wrapped_input_line(
            &mut lines,
            logical_line,
            offset,
            wrap_width,
            selection.as_ref(),
        );
        offset += segment.len();
    }
    if !saw_line || input.ends_with('\n') {
        push_hard_wrapped_input_line(&mut lines, "", input.len(), wrap_width, selection.as_ref());
    }
    lines
}

fn push_hard_wrapped_input_line(
    lines: &mut Vec<Line<'static>>,
    logical_line: &str,
    logical_start: usize,
    wrap_width: usize,
    selection: Option<&Range<usize>>,
) {
    if logical_line.is_empty() {
        lines.push(Line::from(String::new()));
        return;
    }

    let mut current = String::new();
    let mut current_width = 0usize;
    let mut current_start = logical_start;
    let mut current_end = logical_start;
    for (offset, ch) in logical_line.char_indices() {
        current_end = logical_start + offset + ch.len_utf8();
        current.push(ch);
        current_width += 1;
        if current_width == wrap_width {
            lines.push(input_visual_line(
                current,
                current_start,
                current_end,
                selection,
            ));
            current = String::new();
            current_width = 0;
            current_start = current_end;
        }
    }
    if !current.is_empty() {
        lines.push(input_visual_line(
            current,
            current_start,
            current_end,
            selection,
        ));
    }
}

fn input_visual_line(
    text: String,
    line_start: usize,
    line_end: usize,
    selection: Option<&Range<usize>>,
) -> Line<'static> {
    let Some(selection) = selection else {
        return Line::from(ansi_text_spans(&text, Style::default()));
    };
    if selection.start >= line_end || selection.end <= line_start {
        return Line::from(ansi_text_spans(&text, Style::default()));
    }
    let mut spans = Vec::new();
    for (segment, selected) in split_input_line_selection(&text, line_start, selection) {
        let style = if selected {
            Style::default().bg(INPUT_SELECTION_BG)
        } else {
            Style::default()
        };
        spans.extend(ansi_text_spans(&segment, style));
    }
    Line::from(spans)
}

fn split_input_line_selection(
    text: &str,
    line_start: usize,
    selection: &Range<usize>,
) -> Vec<(String, bool)> {
    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut segment_selected = None;
    for (offset, ch) in text.char_indices() {
        let absolute = line_start + offset;
        let selected = selection.start <= absolute && absolute < selection.end;
        match segment_selected {
            Some(current) if current == selected => {}
            Some(current) => {
                segments.push((std::mem::take(&mut segment), current));
                segment_selected = Some(selected);
            }
            None => {
                segment_selected = Some(selected);
            }
        }
        segment.push(ch);
    }
    if let Some(selected) = segment_selected {
        segments.push((segment, selected));
    }
    segments
}

fn wrapped_line_count(line: &str, wrap_width: usize) -> usize {
    let width = wrap_width.max(1);
    let chars = line.chars().count();
    chars.max(1).div_ceil(width)
}

fn cursor_visual_row_col(char_col: usize, wrap_width: usize) -> (usize, usize) {
    let width = wrap_width.max(1);
    (char_col / width, char_col % width)
}

fn input_cursor_position(
    snapshot: &UiSnapshot,
    area: Rect,
    scroll_offset: u16,
) -> Option<Position> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let metrics = input_cursor_metrics(snapshot, area.width);
    Some(Position {
        x: area.x + metrics.col.min(area.width.saturating_sub(1)),
        y: area.y
            + metrics
                .row
                .saturating_sub(scroll_offset)
                .min(area.height.saturating_sub(1)),
    })
}

fn input_byte_index_for_visual_position(
    input: &str,
    wrap_width: u16,
    visual_row: u16,
    visual_col: u16,
) -> usize {
    let wrap_width = wrapped_input_width(wrap_width);
    let mut row = 0usize;
    let target_row = usize::from(visual_row);
    let target_col = usize::from(visual_col);
    let mut offset = 0usize;
    let mut saw_line = false;
    for segment in input.split_inclusive('\n') {
        saw_line = true;
        let logical_line = segment.strip_suffix('\n').unwrap_or(segment);
        let line_rows = wrapped_line_count(logical_line, wrap_width);
        if target_row < row.saturating_add(line_rows) {
            let row_in_line = target_row.saturating_sub(row);
            let char_col = row_in_line
                .saturating_mul(wrap_width)
                .saturating_add(target_col);
            return byte_index_for_char_column(
                input,
                offset,
                offset.saturating_add(logical_line.len()),
                char_col,
            );
        }
        row = row.saturating_add(line_rows);
        offset = offset.saturating_add(segment.len());
    }
    if !saw_line || input.ends_with('\n') {
        return input.len();
    }
    input.len()
}

fn mouse_in_rect(mouse: MouseEvent, area: Rect) -> bool {
    mouse.column >= area.x
        && mouse.column < area.x.saturating_add(area.width)
        && mouse.row >= area.y
        && mouse.row < area.y.saturating_add(area.height)
}

const TOOL_BLOCK_LINE_BUDGET: usize = 96;

fn any_snapshot_subagent_activity_running(snapshot: &UiSnapshot) -> bool {
    !snapshot.active_activities.is_empty()
        || snapshot
            .agents
            .iter()
            .any(|a| matches!(a.status, AgentStatus::Running))
}

#[derive(Debug, Clone)]
struct SubagentActivityFrame {
    entry_id: Option<TranscriptEntryId>,
    title: String,
    status: String,
    detail: String,
}

fn subagent_activity_frame_content(frame: &SubagentActivityFrame) -> String {
    let title = frame.title.trim();
    let status = frame.status.trim();
    let detail = frame.detail.trim();
    format!("┌─ {title} ─\n│ {status}\n│ {detail}\n└")
}

fn ui_areas(area: Rect, input_height: u16, activity_height: u16) -> [Rect; 4] {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(activity_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .areas(area)
}

fn activity_area_height(lines: &[Line<'_>]) -> u16 {
    u16::try_from(lines.len()).unwrap_or(u16::MAX)
}

/// Render a UI snapshot into a Ratatui frame.
pub fn render(frame: &mut Frame<'_>, snapshot: &UiSnapshot) {
    let input_height = input_area_height(frame.area(), snapshot);
    let activity_lines = activity_summary_lines(snapshot, None);
    let [main, agents_area, input, status] = ui_areas(
        frame.area(),
        input_height,
        activity_area_height(&activity_lines),
    );
    let transcript_viewport = transcript_viewport(
        &snapshot.transcript_entries,
        main.height as usize,
        main.width,
        0,
        None,
    );
    let _ = render_with_transcript_scroll(
        frame,
        snapshot,
        transcript_viewport.as_ref(),
        main,
        agents_area,
        input,
        status,
        false,
        true,
        activity_lines,
        None,
        None,
        None,
    );
}

/// Render a stateful TUI app into a Ratatui frame.
pub fn render_app(frame: &mut Frame<'_>, app: &mut TuiApp) {
    let input_height = input_area_height(frame.area(), &app.snapshot);
    let working_elapsed = app.working_elapsed();
    let activity_lines = activity_summary_lines(&app.snapshot, working_elapsed);
    let [main, agents_area, input, status] = ui_areas(
        frame.area(),
        input_height,
        activity_area_height(&activity_lines),
    );
    let input_inner = input_content_area(input);
    let input_scroll = input_scroll_offset(&app.snapshot, input_inner);
    app.update_input_render_metrics(input_inner, input_scroll);
    let transcript_selection = app.transcript_view.selection().cloned();
    let input_selection = app.input_editor.selection_range(&app.snapshot);
    let show_transcript_scrollbar = app.transcript_scrollbar_visible();
    let transcript_viewport = app.transcript_view.viewport_ref(
        main.height as usize,
        main.width,
        transcript_selection.is_some(),
    );
    let metrics = render_with_transcript_scroll(
        frame,
        &app.snapshot,
        transcript_viewport,
        main,
        agents_area,
        input,
        status,
        app.ctrl_c_exit_armed,
        show_transcript_scrollbar,
        activity_lines,
        transcript_selection.as_ref(),
        input_selection,
        app.context_window_usage,
    );
    app.transcript_view.update_render_metrics(metrics);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptRenderMetrics {
    pub(crate) viewport_height: usize,
    pub(crate) scroll_offset: usize,
    pub(crate) transcript_area: Rect,
}

fn render_with_transcript_scroll(
    frame: &mut Frame<'_>,
    snapshot: &UiSnapshot,
    transcript_viewport: TranscriptViewportRef<'_>,
    main: Rect,
    agents_area: Rect,
    input: Rect,
    status: Rect,
    show_exit_warning: bool,
    show_transcript_scrollbar: bool,
    activity_lines: Vec<Line<'static>>,
    transcript_selection: Option<&TranscriptSelection>,
    input_selection: Option<Range<usize>>,
    context_usage: Option<ContextWindowUsage>,
) -> TranscriptRenderMetrics {
    render_transcript_lines(frame, main, transcript_viewport, transcript_selection);
    if show_transcript_scrollbar {
        render_transcript_scrollbar(frame, main, &transcript_viewport);
    }

    render_activity_panel(frame, activity_lines, agents_area);

    let input_margin = input_margin_area(input);
    let input_inner = input_content_area(input);
    let input_scroll = input_scroll_offset(snapshot, input_inner);
    frame.render_widget(Paragraph::new("").style(input_area_style()), input);
    frame.render_widget(
        Paragraph::new(input_margin_lines(
            snapshot,
            input_margin,
            input_scroll,
            input_inner.width,
        ))
        .style(input_area_style()),
        input_margin,
    );
    frame.render_widget(
        Paragraph::new(input_visual_lines_with_selection(
            &snapshot.input,
            input_inner.width,
            input_selection,
        ))
        .style(input_area_style())
        .scroll((input_scroll, 0)),
        input_inner,
    );
    if let Some(position) = input_cursor_position(snapshot, input_inner, input_scroll) {
        frame.set_cursor_position(position);
    }

    frame.render_widget(
        Paragraph::new(status_line_with_context(snapshot, context_usage))
            .style(status_area_style()),
        status,
    );
    if let Some(toast) = toast_line(show_exit_warning, transcript_selection)
        && let Some(area) = toast_area(main, &toast)
    {
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(toast).style(toast_style()), area);
    }

    TranscriptRenderMetrics {
        viewport_height: transcript_viewport.viewport_height.max(1),
        scroll_offset: transcript_viewport.scroll_offset,
        transcript_area: main,
    }
}

fn render_transcript_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    transcript_viewport: &TranscriptViewportRef<'_>,
) {
    if area.width == 0
        || area.height == 0
        || transcript_viewport.total_lines <= transcript_viewport.viewport_height
    {
        return;
    }
    let scrollbar_area = Rect {
        x: area.x.saturating_add(area.width.saturating_sub(1)),
        y: area.y,
        width: 1,
        height: area.height,
    };
    frame.render_widget(
        Paragraph::new(transcript_scrollbar_lines(
            transcript_viewport.total_lines,
            transcript_viewport.viewport_height,
            transcript_viewport.top_line,
            area.height,
        )),
        scrollbar_area,
    );
}

fn render_transcript_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    transcript_viewport: TranscriptViewportRef<'_>,
    selection: Option<&TranscriptSelection>,
) {
    frame.render_widget(Paragraph::new(""), area);
    let visible_rows = transcript_viewport
        .lines
        .len()
        .min(usize::from(area.height));
    for row in 0..visible_rows {
        let row_area = Rect {
            x: area.x,
            y: area
                .y
                .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
            width: area.width,
            height: 1,
        };
        let line = &transcript_viewport.lines[row];
        if let Some(selection) = selection
            && let Some(metadata) = transcript_viewport.metadata.get(row)
            && let Some(content) = metadata.content.as_ref()
        {
            let content_len = content.chars().count();
            if let Some((start, end)) =
                selection_range_for_line(selection, metadata.visual_line, content_len)
            {
                frame.render_widget(
                    apply_selection_to_transcript_line(line.clone(), start, end),
                    row_area,
                );
                continue;
            }
        }
        frame.render_widget(line, row_area);
    }
}

fn transcript_scrollbar_lines(
    total_lines: usize,
    viewport_height: usize,
    top_line: usize,
    area_height: u16,
) -> Vec<Line<'static>> {
    let height = usize::from(area_height);
    if height == 0 || total_lines <= viewport_height {
        return Vec::new();
    }
    let max_top_line = total_lines.saturating_sub(viewport_height.max(1));
    let thumb_row = if max_top_line == 0 || height == 1 {
        0
    } else {
        top_line.min(max_top_line).saturating_mul(height - 1) / max_top_line
    };
    (0..height)
        .map(|row| {
            if row == thumb_row {
                Line::from(Span::styled("█", Style::default().fg(ACTIVITY_PRIMARY_FG)))
            } else {
                Line::from("")
            }
        })
        .collect()
}

fn render_activity_panel(frame: &mut Frame<'_>, lines: Vec<Line<'static>>, area: Rect) {
    if lines.is_empty() || area.height == 0 {
        return;
    }
    frame.render_widget(Paragraph::new("").style(activity_area_style()), area);
    frame.render_widget(
        Paragraph::new(activity_panel_lines(lines)).style(activity_area_style()),
        area,
    );
}

fn activity_area_style() -> Style {
    Style::default().bg(ACTIVITY_AREA_BG)
}

fn activity_panel_lines(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            let mut spans = Vec::with_capacity(line.spans.len() + 2);
            spans.push(Span::styled(
                "│ ",
                Style::default().fg(ACTIVITY_GUTTER_FG).bg(ACTIVITY_AREA_BG),
            ));
            spans.extend(line.spans.into_iter().map(activity_panel_span));
            Line::from(spans)
        })
        .collect()
}

fn activity_panel_span(span: Span<'static>) -> Span<'static> {
    let mut style = span.style;
    style.bg = Some(ACTIVITY_AREA_BG);
    Span::styled(span.content, style)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptViewport {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) metadata: Vec<TranscriptLineMetadata>,
    pub(crate) total_lines: usize,
    pub(crate) viewport_height: usize,
    pub(crate) top_line: usize,
    pub(crate) scroll_offset: usize,
}

impl TranscriptViewport {
    fn as_ref(&self) -> TranscriptViewportRef<'_> {
        TranscriptViewportRef {
            lines: &self.lines,
            metadata: &self.metadata,
            total_lines: self.total_lines,
            viewport_height: self.viewport_height,
            top_line: self.top_line,
            scroll_offset: self.scroll_offset,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptViewportRef<'a> {
    pub(crate) lines: &'a [Line<'static>],
    pub(crate) metadata: &'a [TranscriptLineMetadata],
    pub(crate) total_lines: usize,
    pub(crate) viewport_height: usize,
    pub(crate) top_line: usize,
    pub(crate) scroll_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptViewportPosition {
    pub(crate) total_lines: usize,
    pub(crate) viewport_height: usize,
    pub(crate) top_line: usize,
    pub(crate) scroll_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptLineMetadata {
    pub(crate) visual_line: usize,
    pub(crate) content: Option<String>,
}

pub(crate) fn transcript_viewport_position(
    line_index: &TranscriptLineIndex,
    viewport_height: usize,
    scroll_offset: usize,
) -> TranscriptViewportPosition {
    let total_lines = line_index.total_lines();
    let viewport_height = viewport_height.max(1);
    let max_top_line = total_lines.saturating_sub(viewport_height);
    let scroll_offset = scroll_offset.min(max_top_line);
    let top_line = max_top_line.saturating_sub(scroll_offset);
    TranscriptViewportPosition {
        total_lines,
        viewport_height,
        top_line,
        scroll_offset,
    }
}

fn transcript_viewport(
    entries: &[UiTranscriptEntry],
    viewport_height: usize,
    viewport_width: u16,
    scroll_offset: usize,
    defer_syntax_highlighting_entry: Option<usize>,
) -> TranscriptViewport {
    let (line_index, wrap_width) =
        transcript_line_index_for_viewport(entries, viewport_height, viewport_width);
    let position = transcript_viewport_position(&line_index, viewport_height, scroll_offset);
    let mut lines = Vec::with_capacity(position.viewport_height.min(position.total_lines));
    let mut metadata = Vec::with_capacity(position.viewport_height.min(position.total_lines));
    for visible_range in line_index.visible_ranges(position.top_line, position.viewport_height) {
        let index = visible_range.entry_index;
        let entry = &entries[index];
        let mut render_context = transcript_entry_render_context(entries, index);
        render_context.defer_syntax_highlighting = defer_syntax_highlighting_entry == Some(index);
        let rendered_lines = transcript_entry_lines_range_with_separator(
            entry,
            visible_range.entry_line_start,
            visible_range.line_count,
            visible_range.has_separator,
            render_context.clone(),
            wrap_width,
        );
        for offset in 0..rendered_lines.len() {
            let entry_display_line = visible_range.entry_line_start.saturating_add(offset);
            metadata.push(TranscriptLineMetadata {
                visual_line: visible_range.visual_line_start.saturating_add(offset),
                content: transcript_entry_display_line_content(
                    entry,
                    entry_display_line,
                    render_context.clone(),
                    wrap_width,
                ),
            });
        }
        lines.extend(rendered_lines);
    }

    TranscriptViewport {
        lines,
        metadata,
        total_lines: position.total_lines,
        viewport_height: position.viewport_height,
        top_line: position.top_line,
        scroll_offset: position.scroll_offset,
    }
}

fn transcript_line_index_for_viewport(
    entries: &[UiTranscriptEntry],
    viewport_height: usize,
    viewport_width: u16,
) -> (TranscriptLineIndex, usize) {
    let wrap_width = transcript_content_width(viewport_width);
    let line_index = transcript_line_index(entries, wrap_width);
    if transcript_scrollbar_visible(&line_index, viewport_height, viewport_width) {
        let scrollbar_wrap_width = transcript_content_width_with_scrollbar(viewport_width);
        if scrollbar_wrap_width != wrap_width {
            return (
                transcript_line_index(entries, scrollbar_wrap_width),
                scrollbar_wrap_width,
            );
        }
    }
    (line_index, wrap_width)
}

pub(crate) fn transcript_scrollbar_visible(
    line_index: &TranscriptLineIndex,
    viewport_height: usize,
    viewport_width: u16,
) -> bool {
    viewport_width > 0 && line_index.total_lines() > viewport_height.max(1)
}

pub(crate) fn transcript_line_index(
    entries: &[UiTranscriptEntry],
    wrap_width: usize,
) -> TranscriptLineIndex {
    TranscriptLineIndex::build_with_index_and_separator(
        entries,
        |index, entry| {
            let render_context = transcript_entry_render_context(entries, index);
            transcript_entry_line_count_with_context(entry, render_context, wrap_width)
        },
        |entry_index, next_visible_entry_index| {
            transcript_entries_have_separator(entries, entry_index, next_visible_entry_index)
        },
    )
}

pub(crate) fn transcript_entries_have_separator(
    entries: &[UiTranscriptEntry],
    entry_index: usize,
    next_visible_entry_index: usize,
) -> bool {
    if matches!(
        (
            entries.get(entry_index),
            entries.get(next_visible_entry_index)
        ),
        (
            Some(UiTranscriptEntry::SessionRecord(
                SessionRecordKind::FreeformToolCall(_) | SessionRecordKind::FunctionToolCall(_)
            )),
            Some(UiTranscriptEntry::SessionRecord(
                SessionRecordKind::FreeformToolOutput(_) | SessionRecordKind::FunctionToolOutput(_)
            ))
        )
    ) {
        return false;
    }

    true
}

pub(crate) fn transcript_total_line_count(
    entries: &[UiTranscriptEntry],
    wrap_width: usize,
) -> usize {
    transcript_line_index(entries, wrap_width).total_lines()
}

#[cfg(test)]
fn transcript_total_bytes(entries: &[UiTranscriptEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match entry {
            UiTranscriptEntry::Text(text) => text.len(),
            UiTranscriptEntry::SessionRecord(SessionRecordKind::UserMessage(MessageRecord {
                text,
            }))
            | UiTranscriptEntry::SessionRecord(SessionRecordKind::DeveloperMessage(
                MessageRecord { text },
            ))
            | UiTranscriptEntry::SessionRecord(SessionRecordKind::AssistantMessage(
                MessageRecord { text },
            )) => text.len(),
            UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolCall(call)) => {
                call.call_id.len() + call.name.len() + call.input.len()
            }
            UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolOutput(output)) => {
                output.call_id.len()
                    + output.output.len()
                    + output.display_output.as_deref().unwrap_or_default().len()
            }
            UiTranscriptEntry::SessionRecord(SessionRecordKind::FunctionToolCall(call)) => {
                call.call_id.len() + call.name.len() + call.arguments.len()
            }
            UiTranscriptEntry::SessionRecord(SessionRecordKind::FunctionToolOutput(output)) => {
                output.call_id.len()
                    + output.output.len()
                    + output.display_output.as_deref().unwrap_or_default().len()
            }
            UiTranscriptEntry::SessionRecord(_) => 0,
        })
        .sum()
}

pub(crate) fn transcript_suffix_prefix_overlap<T: PartialEq>(
    incoming: &[T],
    current: &[T],
) -> usize {
    let max_overlap = incoming.len().min(current.len());
    (1..=max_overlap)
        .rev()
        .find(|overlap| incoming[incoming.len() - overlap..] == current[..*overlap])
        .unwrap_or(0)
}

#[cfg(test)]
fn transcript_entry_line_count(entry: &UiTranscriptEntry) -> usize {
    transcript_entry_line_count_with_context(
        entry,
        TranscriptEntryRenderContext::default(),
        usize::MAX,
    )
}

pub(crate) fn transcript_entry_line_count_with_context(
    entry: &UiTranscriptEntry,
    render_context: TranscriptEntryRenderContext,
    wrap_width: usize,
) -> usize {
    transcript_entry_content_lines(entry, render_context, wrap_width).len()
}

fn transcript_entry_lines_range_with_context(
    entry: &UiTranscriptEntry,
    start: usize,
    take: usize,
    render_context: TranscriptEntryRenderContext,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    if take == 0 {
        return Vec::new();
    }
    transcript_entry_lines_with_context(entry, render_context, wrap_width)
        .into_iter()
        .skip(start)
        .take(take)
        .collect()
}

pub(crate) fn transcript_entry_lines_with_context(
    entry: &UiTranscriptEntry,
    render_context: TranscriptEntryRenderContext,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    match entry {
        UiTranscriptEntry::Text(text) => {
            let (kind, text) = classify_transcript_entry(text);
            let render_options = if render_context.defer_syntax_highlighting {
                TextRenderOptions::PLAIN
            } else {
                TextRenderOptions::HIGHLIGHTED
            };
            decorate_transcript_lines(
                kind,
                wrap_content_lines(text_lines_with_options(text, render_options), wrap_width),
            )
        }
        UiTranscriptEntry::SessionRecord(kind) => {
            transcript_session_record_lines(kind, render_context, wrap_width)
        }
    }
}

fn transcript_entry_lines_range_with_separator(
    entry: &UiTranscriptEntry,
    start: usize,
    take: usize,
    has_separator: bool,
    render_context: TranscriptEntryRenderContext,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    if take == 0 {
        return Vec::new();
    }
    let entry_lines =
        transcript_entry_line_count_with_context(entry, render_context.clone(), wrap_width);
    let end = start.saturating_add(take);
    let mut lines = Vec::with_capacity(take);
    if start < entry_lines {
        let body_take = end.min(entry_lines).saturating_sub(start);
        lines.extend(transcript_entry_lines_range_with_context(
            entry,
            start,
            body_take,
            render_context.clone(),
            wrap_width,
        ));
    }
    if has_separator && start <= entry_lines && end > entry_lines {
        lines.push(Line::from(String::new()));
    }
    lines
}

pub(crate) fn selection_range_for_line(
    selection: &TranscriptSelection,
    visual_line: usize,
    content_len: usize,
) -> Option<(usize, usize)> {
    let (start, end) = normalized_selection_points(selection);
    if visual_line < start.visual_line || visual_line > end.visual_line {
        return None;
    }
    let range_start = if visual_line == start.visual_line {
        start.content_col.min(content_len)
    } else {
        0
    };
    let range_end = if visual_line == end.visual_line {
        end.content_col.min(content_len)
    } else {
        content_len
    };
    (range_start < range_end).then_some((range_start, range_end))
}

pub(crate) fn normalized_selection_points(
    selection: &TranscriptSelection,
) -> (TranscriptSelectionPoint, TranscriptSelectionPoint) {
    if selection.anchor <= selection.cursor {
        (selection.anchor, selection.cursor)
    } else {
        (selection.cursor, selection.anchor)
    }
}

fn apply_selection_to_transcript_line(
    line: Line<'static>,
    selection_start: usize,
    selection_end: usize,
) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    let mut content_cursor = 0usize;
    for (index, span) in line.spans.into_iter().enumerate() {
        if index < 3 {
            spans.push(span);
            continue;
        }
        let span_text = span.content.to_string();
        let span_len = span_text.chars().count();
        let span_start = content_cursor;
        let span_end = content_cursor.saturating_add(span_len);
        append_selected_span_segments(
            &mut spans,
            &span_text,
            span.style,
            selection_start.saturating_sub(span_start).min(span_len),
            selection_end.saturating_sub(span_start).min(span_len),
            selection_start < span_end && selection_end > span_start,
        );
        content_cursor = span_end;
    }
    Line::from(spans)
}

fn append_selected_span_segments(
    target: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    local_start: usize,
    local_end: usize,
    intersects: bool,
) {
    if !intersects {
        target.push(Span::styled(text.to_string(), style));
        return;
    }
    if local_start > 0 {
        target.push(Span::styled(chars_range(text, 0, local_start), style));
    }
    if local_start < local_end {
        target.push(Span::styled(
            chars_range(text, local_start, local_end),
            style.add_modifier(Modifier::REVERSED),
        ));
    }
    let text_len = text.chars().count();
    if local_end < text_len {
        target.push(Span::styled(chars_range(text, local_end, text_len), style));
    }
}

pub(crate) fn chars_range(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

pub(crate) fn transcript_entry_display_line_content(
    entry: &UiTranscriptEntry,
    line_offset: usize,
    render_context: TranscriptEntryRenderContext,
    wrap_width: usize,
) -> Option<String> {
    let line_count =
        transcript_entry_line_count_with_context(entry, render_context.clone(), wrap_width);
    if line_offset >= line_count {
        return None;
    }
    Some(
        transcript_entry_content_lines(entry, render_context, wrap_width)
            .into_iter()
            .nth(line_offset)
            .unwrap_or_default(),
    )
}

#[cfg(test)]
fn transcript_content_line_at_indexed(
    entries: &[UiTranscriptEntry],
    line_index: &TranscriptLineIndex,
    visual_line: usize,
) -> Option<String> {
    let address = line_index.line_address(visual_line)?;
    if address.entry_line >= address.body_lines {
        return None;
    }
    let entry = entries.get(address.entry_index)?;
    let render_context = transcript_entry_render_context(entries, address.entry_index);
    transcript_entry_display_line_content(entry, address.entry_line, render_context, usize::MAX)
}

pub(crate) fn transcript_entry_content_lines(
    entry: &UiTranscriptEntry,
    render_context: TranscriptEntryRenderContext,
    wrap_width: usize,
) -> Vec<String> {
    match entry {
        UiTranscriptEntry::Text(text) => {
            let (_, text) = classify_transcript_entry(text);
            hard_wrap_plain_lines(text_content_lines(text), wrap_width)
        }
        UiTranscriptEntry::SessionRecord(kind) => {
            transcript_session_record_lines(kind, render_context, wrap_width)
                .iter()
                .map(line_plain_text)
                .collect()
        }
    }
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

#[cfg(test)]
fn selected_transcript_text(
    entries: &[UiTranscriptEntry],
    selection: &TranscriptSelection,
) -> Option<String> {
    let (start, end) = normalized_selection_points(selection);
    if start == end {
        return None;
    }
    let line_index = transcript_line_index(entries, usize::MAX);
    let mut lines = Vec::new();
    for visual_line in start.visual_line..=end.visual_line {
        match transcript_content_line_at_indexed(entries, &line_index, visual_line) {
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

fn trim_transcript_entry(entry: &mut String) {
    if entry.len() <= MAX_TRANSCRIPT_ENTRY_BYTES {
        if entry.capacity() > MAX_TRANSCRIPT_ENTRY_BYTES {
            entry.shrink_to_fit();
        }
        return;
    }
    let prefix_len = transcript_prefix_len(entry);
    let marker = TRANSCRIPT_TRUNCATED_MARKER;
    let tail_budget = MAX_TRANSCRIPT_ENTRY_BYTES
        .saturating_sub(prefix_len)
        .saturating_sub(marker.len())
        .max(1);
    let tail_start = floor_char_boundary_from_end(entry, tail_budget);
    let prefix = entry[..prefix_len].to_string();
    let tail = entry[tail_start..].to_string();
    let mut trimmed = String::with_capacity(prefix.len() + marker.len() + tail.len());
    trimmed.push_str(&prefix);
    trimmed.push_str(marker);
    trimmed.push_str(&tail);
    *entry = trimmed;
}

fn transcript_prefix_len(entry: &str) -> usize {
    for prefix in ["assistant: ", "assistant> ", "developer> ", "user> ", "> "] {
        if entry.starts_with(prefix) {
            return prefix.len();
        }
    }
    0
}

fn floor_char_boundary_from_end(text: &str, max_tail_bytes: usize) -> usize {
    let mut index = text.len().saturating_sub(max_tail_bytes);
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn activity_summary_lines(
    snapshot: &UiSnapshot,
    working_elapsed: Option<Duration>,
) -> Vec<Line<'static>> {
    const ACTIVITY_LINE_BUDGET: usize = 3;

    let mut lines = Vec::new();
    if let Some(elapsed) = working_elapsed {
        lines.push(working_line(elapsed, snapshot.response_streaming));
    }
    if let Some(prompt) = snapshot.queued_steering_prompt.as_ref() {
        lines.extend(queued_steering_lines(prompt));
    }

    let remaining = ACTIVITY_LINE_BUDGET.saturating_sub(lines.len());
    lines.extend(agent_summary_lines(snapshot, remaining));
    lines
}

fn working_line(elapsed: Duration, interruptible: bool) -> Line<'static> {
    let detail = if interruptible {
        format!("({} • esc to interrupt)", elapsed_label(elapsed))
    } else {
        format!("({})", elapsed_label(elapsed))
    };
    Line::from(vec![
        Span::styled("• Working ", Style::default().fg(ACTIVITY_PRIMARY_FG)),
        Span::styled(detail, Style::default().fg(ACTIVITY_SECONDARY_FG)),
    ])
}

fn queued_steering_lines(prompt: &str) -> Vec<Line<'static>> {
    let prompt_lines: Vec<&str> = prompt.lines().collect();
    prompt_lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let last = index + 1 == prompt_lines.len();
            let mut spans = Vec::new();
            if index == 0 {
                spans.push(Span::styled(
                    "queued ",
                    Style::default().fg(ACTIVITY_SECONDARY_FG),
                ));
            } else {
                spans.push(Span::styled(
                    "       ",
                    Style::default().fg(ACTIVITY_SECONDARY_FG),
                ));
            }
            spans.push(single_ansi_text_span(
                line,
                Style::default().fg(ACTIVITY_QUEUED_FG),
            ));
            if last {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    "(esc to steer instantly)",
                    Style::default().fg(ACTIVITY_SECONDARY_FG),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn elapsed_label(elapsed: Duration) -> String {
    format!("{}s", elapsed.as_secs())
}

fn agent_summary_lines(snapshot: &UiSnapshot, max_lines: usize) -> Vec<Line<'static>> {
    let agent_count = snapshot.agents.len();
    if agent_count == 0 || max_lines == 0 {
        return Vec::new();
    }
    if agent_count <= max_lines {
        return snapshot
            .agents
            .iter()
            .map(agent_summary_line)
            .collect::<Vec<_>>();
    }
    if max_lines == 1 {
        let agent = &snapshot.agents[0];
        return vec![Line::from(vec![
            Span::styled("agent ", Style::default().fg(ACTIVITY_SECONDARY_FG)),
            single_ansi_text_span(&agent.path, Style::default().fg(ACTIVITY_AGENT_PATH_FG)),
            Span::styled(
                format!(" and {} more", agent_count - 1),
                Style::default().fg(ACTIVITY_SECONDARY_FG),
            ),
        ])];
    }

    let visible_agents = max_lines - 1;
    let mut lines = snapshot
        .agents
        .iter()
        .take(visible_agents)
        .map(agent_summary_line)
        .collect::<Vec<_>>();
    lines.push(Line::from(Span::styled(
        format!("and {} more", agent_count - visible_agents),
        Style::default().fg(ACTIVITY_SECONDARY_FG),
    )));
    lines
}

fn agent_summary_line(agent: &harness_core::subagents::AgentSummary) -> Line<'static> {
    let status = match &agent.status {
        AgentStatus::Running => "running".to_string(),
        AgentStatus::Waiting => "waiting".to_string(),
        AgentStatus::Completed(_) => "completed".to_string(),
        AgentStatus::Failed(message) => format!("failed: {}", compact_agent_status(message)),
        AgentStatus::Interrupted => "interrupted".to_string(),
    };
    let activity = agent.last_activity_message.as_deref().unwrap_or_default();
    Line::from(vec![
        Span::styled("agent ", Style::default().fg(ACTIVITY_SECONDARY_FG)),
        single_ansi_text_span(&agent.path, Style::default().fg(ACTIVITY_AGENT_PATH_FG)),
        Span::raw(" "),
        single_ansi_text_span(status, Style::default().fg(ACTIVITY_SECONDARY_FG)),
        Span::raw(if activity.is_empty() { "" } else { " " }),
        single_ansi_text_span(activity, Style::default().fg(ACTIVITY_SECONDARY_FG)),
    ])
}

fn compact_agent_status(message: &str) -> String {
    const MAX_AGENT_STATUS_CHARS: usize = 120;

    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX_AGENT_STATUS_CHARS {
        return normalized;
    }
    let mut output = normalized
        .chars()
        .take(MAX_AGENT_STATUS_CHARS.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

#[cfg(test)]
fn transcript_entry_lines(entry: UiTranscriptEntry) -> Vec<Line<'static>> {
    transcript_entry_lines_with_context(&entry, TranscriptEntryRenderContext::default(), usize::MAX)
}

#[cfg(test)]
fn text_entry(text: &str) -> UiTranscriptEntry {
    UiTranscriptEntry::Text(text.to_string())
}

#[cfg(test)]
fn freeform_tool_call(name: &str, input: &str) -> UiTranscriptEntry {
    UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolCall(
        FreeformToolCallRecord {
            call_id: "call-1".to_string(),
            name: name.to_string(),
            input: input.to_string(),
        },
    ))
}

#[cfg(test)]
fn freeform_tool_output(output: &str) -> UiTranscriptEntry {
    UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolOutput(
        FreeformToolOutputRecord {
            call_id: "call-1".to_string(),
            output: output.to_string(),
            display_output: None,
            display: None,
        },
    ))
}
#[cfg(test)]
fn freeform_tool_output_with_structured_display(
    output: &str,
    display: ToolOutputDisplayRecord,
) -> UiTranscriptEntry {
    UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolOutput(
        FreeformToolOutputRecord {
            call_id: "call-1".to_string(),
            output: output.to_string(),
            display_output: Some(String::new()),
            display: Some(display),
        },
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct TranscriptEntryRenderContext {
    pub(crate) failed_apply_patch_call: bool,
    pub(crate) failed_apply_patch_output: bool,
    pub(crate) successful_apply_patch_output: bool,
    pub(crate) failed_edit_file_call: bool,
    pub(crate) failed_edit_file_output: bool,
    pub(crate) successful_edit_file_output: bool,
    pub(crate) defer_syntax_highlighting: bool,
    terminal_output_context: Option<TerminalOutputContext>,
    terminal_echoed_input: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ToolCallView<'a> {
    kind: ToolCallKind,
    name: &'a str,
    input: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCallKind {
    ApplyPatch,
    EditFile,
    Inspect,
    Terminal(TerminalToolKind),
    Other,
}

impl ToolCallKind {
    fn from_name(name: &str) -> Self {
        match name {
            "apply_patch" => Self::ApplyPatch,
            "edit_file" => Self::EditFile,
            "inspect" => Self::Inspect,
            "terminal_open" => Self::Terminal(TerminalToolKind::Open),
            "terminal_write" => Self::Terminal(TerminalToolKind::Write),
            "terminal_read" => Self::Terminal(TerminalToolKind::Read),
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalToolKind {
    Open,
    Write,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TerminalOutputContext {
    Hidden,
    Display,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalCommandIntent {
    FileRead,
    Search,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolOutputEnvelopeView<'a> {
    status: Option<String>,
    token_count: Option<&'a str>,
    body: &'a str,
}

pub(crate) fn transcript_entry_render_context(
    entries: &[UiTranscriptEntry],
    index: usize,
) -> TranscriptEntryRenderContext {
    let entry = entries.get(index);
    let next = entries.get(index.saturating_add(1));
    let previous = index
        .checked_sub(1)
        .and_then(|previous| entries.get(previous));

    let failed_apply_patch_call = match (entry, next) {
        (
            Some(UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolCall(call))),
            Some(UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolOutput(output))),
        ) if call.name == "apply_patch" && call.call_id == output.call_id => {
            is_failed_apply_patch_output(output.transcript_output())
        }
        _ => false,
    };
    let apply_patch_output = match (previous, entry) {
        (
            Some(UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolCall(call))),
            Some(UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolOutput(output))),
        ) if call.name == "apply_patch" && call.call_id == output.call_id => Some(output),
        _ => None,
    };
    let failed_apply_patch_output = apply_patch_output
        .is_some_and(|output| is_failed_apply_patch_output(output.transcript_output()));
    let successful_apply_patch_output = apply_patch_output
        .is_some_and(|output| !is_failed_apply_patch_output(output.transcript_output()));

    let failed_edit_file_call = match (entry, next) {
        (
            Some(UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolCall(call))),
            Some(UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolOutput(output))),
        ) if call.name == "edit_file" && call.call_id == output.call_id => {
            is_failed_edit_file_output(output.transcript_output())
        }
        _ => false,
    };
    let edit_file_output = match (previous, entry) {
        (
            Some(UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolCall(call))),
            Some(UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolOutput(output))),
        ) if call.name == "edit_file" && call.call_id == output.call_id => Some(output),
        _ => None,
    };
    let failed_edit_file_output = edit_file_output
        .is_some_and(|output| is_failed_edit_file_output(output.transcript_output()));
    let successful_edit_file_output = edit_file_output
        .is_some_and(|output| is_successful_edit_file_output(output.transcript_output()));

    let previous_terminal_call = match previous {
        Some(UiTranscriptEntry::SessionRecord(SessionRecordKind::FreeformToolCall(call))) => {
            let kind = ToolCallKind::from_name(&call.name);
            matches!(kind, ToolCallKind::Terminal(_)).then_some((kind, call.input.as_str()))
        }
        _ => None,
    };
    let terminal_output_context = previous_terminal_call
        .and_then(|(kind, input)| terminal_output_context_for_tool_call(kind, input));
    let terminal_echoed_input = previous_terminal_call
        .and_then(|(kind, input)| terminal_submitted_input_for_tool_call(kind, input));

    TranscriptEntryRenderContext {
        failed_apply_patch_call,
        failed_apply_patch_output,
        successful_apply_patch_output,
        failed_edit_file_call,
        failed_edit_file_output,
        successful_edit_file_output,
        defer_syntax_highlighting: false,
        terminal_output_context,
        terminal_echoed_input,
    }
}

fn is_failed_apply_patch_output(output: &str) -> bool {
    !output.trim_start().starts_with("Success.")
}

fn is_failed_edit_file_output(output: &str) -> bool {
    output.trim_start().starts_with("edit errors")
}

fn is_successful_edit_file_output(output: &str) -> bool {
    output.trim() == "ok"
}
fn transcript_session_record_lines(
    record: &SessionRecordKind,
    render_context: TranscriptEntryRenderContext,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    match record {
        SessionRecordKind::UserMessage(MessageRecord { text }) => {
            decorate_transcript_lines(TranscriptKind::User, text_lines(text))
        }
        SessionRecordKind::DeveloperMessage(MessageRecord { text }) => {
            decorate_transcript_lines(TranscriptKind::Developer, text_lines(text))
        }
        SessionRecordKind::AssistantMessage(MessageRecord { text }) => {
            let options = if render_context.defer_syntax_highlighting {
                TextRenderOptions::PLAIN
            } else {
                TextRenderOptions::HIGHLIGHTED
            };
            decorate_transcript_lines(
                TranscriptKind::Assistant,
                wrap_content_lines(text_lines_with_options(text, options), wrap_width),
            )
        }
        SessionRecordKind::FreeformToolCall(call) => decorate_transcript_lines(
            TranscriptKind::Tool,
            wrap_content_lines(
                render_freeform_tool_call_record(call, &render_context),
                wrap_width,
            ),
        ),
        SessionRecordKind::FunctionToolCall(call) => decorate_transcript_lines(
            TranscriptKind::Tool,
            wrap_content_lines(
                render_function_tool_call_record(call, &render_context),
                wrap_width,
            ),
        ),
        SessionRecordKind::FreeformToolOutput(output) => {
            let lines = if let Some(display) = output.display.as_ref() {
                structured_tool_output_display_lines(display, wrap_width)
            } else {
                tool_output_display_lines(
                    output.transcript_output(),
                    render_context.failed_apply_patch_output
                        || render_context.failed_edit_file_output,
                    render_context.successful_apply_patch_output
                        || render_context.successful_edit_file_output,
                    render_context.terminal_output_context,
                    render_context.terminal_echoed_input.as_deref(),
                    wrap_width,
                )
            };
            decorate_transcript_lines(TranscriptKind::Tool, lines)
        }
        SessionRecordKind::FunctionToolOutput(output) => decorate_transcript_lines(
            TranscriptKind::Tool,
            tool_output_display_lines(
                output.transcript_output(),
                render_context.failed_apply_patch_output || render_context.failed_edit_file_output,
                render_context.successful_apply_patch_output
                    || render_context.successful_edit_file_output,
                render_context.terminal_output_context,
                render_context.terminal_echoed_input.as_deref(),
                wrap_width,
            ),
        ),
        SessionRecordKind::SessionClosed(record) => decorate_transcript_lines(
            TranscriptKind::Event,
            text_lines(&format!("session closed: {}", record.closed_at_ms)),
        ),
        SessionRecordKind::SessionMeta(_)
        | SessionRecordKind::TurnContext(_)
        | SessionRecordKind::FreeformToolInputDelta(_)
        | SessionRecordKind::CompactionCheckpoint(_)
        | SessionRecordKind::ProviderSessionBinding(_) => Vec::new(),
    }
}

fn render_freeform_tool_call_record(
    call: &FreeformToolCallRecord,
    render_context: &TranscriptEntryRenderContext,
) -> Vec<Line<'static>> {
    render_tool_call(
        ToolCallView {
            kind: ToolCallKind::from_name(&call.name),
            name: &call.name,
            input: &call.input,
        },
        render_context.failed_apply_patch_call || render_context.failed_edit_file_call,
    )
}

fn render_function_tool_call_record(
    call: &FunctionToolCallRecord,
    render_context: &TranscriptEntryRenderContext,
) -> Vec<Line<'static>> {
    render_tool_call(
        ToolCallView {
            kind: ToolCallKind::from_name(&call.name),
            name: &call.name,
            input: &call.arguments,
        },
        render_context.failed_apply_patch_call || render_context.failed_edit_file_call,
    )
}

fn render_tool_call(call: ToolCallView<'_>, failed_apply_patch_call: bool) -> Vec<Line<'static>> {
    match call.kind {
        ToolCallKind::ApplyPatch => {
            render_apply_patch_tool_call(call.input, failed_apply_patch_call)
        }
        ToolCallKind::EditFile => render_edit_file_tool_call(call.input, failed_apply_patch_call),
        ToolCallKind::Inspect => render_inspect_tool_call(call.input),
        ToolCallKind::Terminal(kind) => {
            terminal_tool_call_lines(kind, FreeformToolInput::new(call.input))
        }
        ToolCallKind::Other => render_generic_tool_call(call),
    }
}

fn render_apply_patch_tool_call(input: &str, failed_apply_patch_call: bool) -> Vec<Line<'static>> {
    let mut lines = if input.is_empty() {
        vec![Line::from("patch")]
    } else if let Some(lines) = apply_patch_edit_lines(input) {
        lines
    } else {
        let mut lines = Vec::new();
        append_with_budget(&mut lines, tool_input_lines(input), TOOL_BLOCK_LINE_BUDGET);
        lines
    };
    if failed_apply_patch_call {
        apply_failed_patch_style(&mut lines);
    }
    lines
}

fn render_inspect_tool_call(input: &str) -> Vec<Line<'static>> {
    let input_lines = input.lines().collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut index = 0usize;

    while index < input_lines.len() {
        let line = input_lines[index].trim();
        if line.is_empty() {
            index += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("read ") {
            let path = path.trim();
            index += 1;
            while index < input_lines.len() {
                let range = input_lines[index].trim();
                if range.is_empty() {
                    index += 1;
                    continue;
                }
                if inspect_command_start_for_display(range) {
                    break;
                }
                if let Some((start, end)) = parse_inspect_read_display_range(range) {
                    lines.push(render_inspect_read_line(path, start, end));
                } else {
                    lines.extend(tool_input_lines(range));
                }
                index += 1;
            }
            continue;
        }

        if inspect_command_start_for_display(line) {
            lines.extend(render_command_tool_call(line));
        } else {
            lines.extend(tool_input_lines(line));
        }
        index += 1;
    }

    lines
}

fn inspect_command_start_for_display(line: &str) -> bool {
    matches!(line, "pwd" | "search" | "which" | "check" | "pgrep" | "ps")
        || line.starts_with("read ")
        || line.starts_with("search ")
        || line.starts_with("which ")
        || line.starts_with("check ")
        || line.starts_with("pgrep ")
        || line.starts_with("ps ")
}

fn parse_inspect_read_display_range(range: &str) -> Option<(usize, usize)> {
    if let Some((start, count)) = range.split_once('+') {
        let start = start.trim().parse::<usize>().ok()?;
        let count = count.trim().parse::<usize>().ok()?;
        if start == 0 || count == 0 {
            return None;
        }
        return Some((start, start.saturating_add(count).saturating_sub(1)));
    }

    let (start, end) = range.split_once('-')?;
    let start = start.trim().parse::<usize>().ok()?;
    let end = end.trim().parse::<usize>().ok()?;
    (start > 0 && end >= start).then_some((start, end))
}

fn render_inspect_read_line(path: &str, start: usize, end: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "Read",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(path.to_string(), Style::default().fg(TRANSCRIPT_TOOL_FG)),
        Span::styled(
            format!(":{start}-{end}"),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn render_edit_file_tool_call(input: &str, failed_edit_file_call: bool) -> Vec<Line<'static>> {
    let mut lines = edit_file_edit_lines(input).unwrap_or_else(|| {
        let mut lines = Vec::new();
        append_with_budget(&mut lines, tool_input_lines(input), TOOL_BLOCK_LINE_BUDGET);
        lines
    });
    if failed_edit_file_call {
        apply_failed_patch_style(&mut lines);
    }
    lines
}

fn edit_file_edit_lines(input: &str) -> Option<Vec<Line<'static>>> {
    let changes = parse_edit_file_display_changes(input)?;
    let mut lines = Vec::new();
    render_apply_patch_edit_summary(&mut lines, &changes);
    let body = render_apply_patch_edit_body(&changes);
    append_with_head_tail_budget(&mut lines, body, TOOL_BLOCK_LINE_BUDGET);
    Some(lines)
}

fn parse_edit_file_display_changes(input: &str) -> Option<Vec<ApplyPatchDisplayChange>> {
    let raw_lines = input.lines().collect::<Vec<_>>();
    let mut index = 0usize;
    let mut changes = Vec::new();

    while index < raw_lines.len() {
        let line = raw_lines[index];
        if line.trim().is_empty() {
            index += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Add ") {
            index += 1;
            let body_start = index;
            while index < raw_lines.len() && !edit_file_top_level_display_header(raw_lines[index]) {
                index += 1;
            }
            let mut change =
                ApplyPatchDisplayChange::new(ApplyPatchDisplayChangeKind::Add, path.trim());
            push_edit_file_added_display_lines(&mut change, &raw_lines[body_start..index]);
            changes.push(change);
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Remove ") {
            changes.push(ApplyPatchDisplayChange::new(
                ApplyPatchDisplayChangeKind::Delete,
                path.trim(),
            ));
            index += 1;
            continue;
        }

        if let Some(from) = line.strip_prefix("*** Move ") {
            index += 1;
            let to = raw_lines.get(index)?.strip_prefix("*** To ")?.trim();
            let mut change =
                ApplyPatchDisplayChange::new(ApplyPatchDisplayChangeKind::Update, from.trim());
            change.move_path = Some(to.to_string());
            changes.push(change);
            index += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Edit ") {
            index += 1;
            let mut change =
                ApplyPatchDisplayChange::new(ApplyPatchDisplayChangeKind::Update, path.trim());
            while index < raw_lines.len() && !edit_file_top_level_display_header(raw_lines[index]) {
                let segment = raw_lines[index];
                if segment.trim().is_empty() {
                    index += 1;
                    continue;
                }
                if let Some(rest) = segment.strip_prefix("*** Replace ") {
                    let (start, end) = edit_file_display_anchor_range(rest)?;
                    change.removed += edit_file_display_removed_count(start, end);
                    change.lines.push(ApplyPatchDisplayLine {
                        kind: ApplyPatchDisplayLineKind::Hunk,
                        text: format!("@@ lines {start}-{end}"),
                    });
                    index += 1;
                    let body_start = index;
                    while index < raw_lines.len()
                        && !edit_file_display_segment_header(raw_lines[index])
                        && !edit_file_top_level_display_header(raw_lines[index])
                    {
                        index += 1;
                    }
                    push_edit_file_added_display_lines(&mut change, &raw_lines[body_start..index]);
                    continue;
                }
                if let Some(rest) = segment.strip_prefix("*** Delete ") {
                    let (start, end) = edit_file_display_anchor_range(rest)?;
                    change.removed += edit_file_display_removed_count(start, end);
                    change.lines.push(ApplyPatchDisplayLine {
                        kind: ApplyPatchDisplayLineKind::Hunk,
                        text: format!("@@ lines {start}-{end}"),
                    });
                    index += 1;
                    continue;
                }
                if let Some(rest) = segment
                    .strip_prefix("*** Before ")
                    .or_else(|| segment.strip_prefix("*** After "))
                    .or_else(|| segment.strip_prefix("*** Append "))
                {
                    let anchor = edit_file_display_anchor_line(rest.trim())?;
                    change.lines.push(ApplyPatchDisplayLine {
                        kind: ApplyPatchDisplayLineKind::Hunk,
                        text: format!("@@ line {anchor}"),
                    });
                    index += 1;
                    let body_start = index;
                    while index < raw_lines.len()
                        && !edit_file_display_segment_header(raw_lines[index])
                        && !edit_file_top_level_display_header(raw_lines[index])
                    {
                        index += 1;
                    }
                    push_edit_file_added_display_lines(&mut change, &raw_lines[body_start..index]);
                    continue;
                }
                return None;
            }
            changes.push(change);
            continue;
        }

        return None;
    }

    (!changes.is_empty()).then_some(changes)
}

fn push_edit_file_added_display_lines(change: &mut ApplyPatchDisplayChange, lines: &[&str]) {
    change.added += lines.len();
    change
        .lines
        .extend(lines.iter().map(|line| ApplyPatchDisplayLine {
            kind: ApplyPatchDisplayLineKind::Add,
            text: (*line).to_string(),
        }));
}

fn edit_file_display_anchor_range(rest: &str) -> Option<(usize, usize)> {
    let mut parts = rest.split_whitespace();
    let start = edit_file_display_anchor_line(parts.next()?)?;
    let end = edit_file_display_anchor_line(parts.next()?)?;
    Some((start, end))
}

fn edit_file_display_anchor_line(anchor: &str) -> Option<usize> {
    let anchor = anchor.trim();
    let split_at = anchor.len().checked_sub(2)?;
    let (line, hash) = anchor.split_at(split_at);
    (!line.is_empty()
        && line.chars().all(|character| character.is_ascii_digit())
        && hash.chars().all(|character| character.is_ascii_hexdigit()))
    .then(|| line.parse::<usize>().ok())
    .flatten()
}

fn edit_file_display_removed_count(start: usize, end: usize) -> usize {
    end.saturating_sub(start).saturating_add(1)
}

fn edit_file_top_level_display_header(line: &str) -> bool {
    line.starts_with("*** Add ")
        || line.starts_with("*** Remove ")
        || line.starts_with("*** Move ")
        || line.starts_with("*** Edit ")
}

fn edit_file_display_segment_header(line: &str) -> bool {
    line.starts_with("*** Replace ")
        || line.starts_with("*** Delete ")
        || line.starts_with("*** Before ")
        || line.starts_with("*** After ")
        || line.starts_with("*** Append ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApplyPatchDisplayChange {
    kind: ApplyPatchDisplayChangeKind,
    path: String,
    move_path: Option<String>,
    added: usize,
    removed: usize,
    lines: Vec<ApplyPatchDisplayLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyPatchDisplayChangeKind {
    Add,
    Delete,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApplyPatchDisplayLine {
    kind: ApplyPatchDisplayLineKind,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyPatchDisplayLineKind {
    Add,
    Delete,
    Context,
    Hunk,
}

fn apply_patch_edit_lines(input: &str) -> Option<Vec<Line<'static>>> {
    let changes = parse_apply_patch_display_changes(input)?;
    let mut lines = Vec::new();
    render_apply_patch_edit_summary(&mut lines, &changes);
    let body = render_apply_patch_edit_body(&changes);
    append_with_head_tail_budget(&mut lines, body, TOOL_BLOCK_LINE_BUDGET);
    Some(lines)
}

fn parse_apply_patch_display_changes(input: &str) -> Option<Vec<ApplyPatchDisplayChange>> {
    let mut changes = Vec::new();
    let mut current: Option<ApplyPatchDisplayChange> = None;
    let mut saw_begin = false;

    for line in input.lines() {
        if line == "*** Begin Patch" {
            saw_begin = true;
            continue;
        }
        if line == "*** End Patch" {
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            push_apply_patch_display_change(&mut changes, &mut current);
            current = Some(ApplyPatchDisplayChange::new(
                ApplyPatchDisplayChangeKind::Add,
                path,
            ));
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            push_apply_patch_display_change(&mut changes, &mut current);
            current = Some(ApplyPatchDisplayChange::new(
                ApplyPatchDisplayChangeKind::Delete,
                path,
            ));
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            push_apply_patch_display_change(&mut changes, &mut current);
            current = Some(ApplyPatchDisplayChange::new(
                ApplyPatchDisplayChangeKind::Update,
                path,
            ));
            continue;
        }

        let Some(change) = current.as_mut() else {
            continue;
        };
        if let Some(move_path) = line.strip_prefix("*** Move to: ") {
            change.move_path = Some(move_path.to_string());
            continue;
        }
        if line == "*** End of File" {
            continue;
        }
        if line.starts_with("@@") {
            change.lines.push(ApplyPatchDisplayLine {
                kind: ApplyPatchDisplayLineKind::Hunk,
                text: line.to_string(),
            });
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            change.added += 1;
            change.lines.push(ApplyPatchDisplayLine {
                kind: ApplyPatchDisplayLineKind::Add,
                text: text.to_string(),
            });
            continue;
        }
        if let Some(text) = line.strip_prefix('-') {
            change.removed += 1;
            change.lines.push(ApplyPatchDisplayLine {
                kind: ApplyPatchDisplayLineKind::Delete,
                text: text.to_string(),
            });
            continue;
        }
        if let Some(text) = line.strip_prefix(' ') {
            change.lines.push(ApplyPatchDisplayLine {
                kind: ApplyPatchDisplayLineKind::Context,
                text: text.to_string(),
            });
        }
    }

    push_apply_patch_display_change(&mut changes, &mut current);
    (saw_begin && !changes.is_empty()).then_some(changes)
}

impl ApplyPatchDisplayChange {
    fn new(kind: ApplyPatchDisplayChangeKind, path: &str) -> Self {
        Self {
            kind,
            path: path.to_string(),
            move_path: None,
            added: 0,
            removed: 0,
            lines: Vec::new(),
        }
    }

    fn verb(&self) -> &'static str {
        match self.kind {
            ApplyPatchDisplayChangeKind::Add => "Added",
            ApplyPatchDisplayChangeKind::Delete => "Deleted",
            ApplyPatchDisplayChangeKind::Update => "Edited",
        }
    }

    fn display_path(&self) -> String {
        match &self.move_path {
            Some(move_path) => format!("{} → {move_path}", self.path),
            None => self.path.clone(),
        }
    }

    fn highlight_path(&self) -> &str {
        self.move_path.as_deref().unwrap_or(&self.path)
    }
}

fn push_apply_patch_display_change(
    changes: &mut Vec<ApplyPatchDisplayChange>,
    current: &mut Option<ApplyPatchDisplayChange>,
) {
    if let Some(change) = current.take() {
        changes.push(change);
    }
}

fn render_apply_patch_edit_summary(
    lines: &mut Vec<Line<'static>>,
    changes: &[ApplyPatchDisplayChange],
) {
    let total_added = changes.iter().map(|change| change.added).sum::<usize>();
    let total_removed = changes.iter().map(|change| change.removed).sum::<usize>();
    if let [change] = changes {
        let mut spans = vec![
            Span::styled("• ", Style::default().fg(Color::DarkGray)),
            Span::styled(change.verb(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            single_ansi_text_span(change.display_path(), Style::default()),
            Span::raw(" "),
        ];
        spans.extend(apply_patch_line_count_spans(change.added, change.removed));
        lines.push(Line::from(spans));
        return;
    }

    let noun = if changes.len() == 1 { "file" } else { "files" };
    let mut spans = vec![
        Span::styled("• ", Style::default().fg(Color::DarkGray)),
        Span::styled("Edited", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(" {} {noun} ", changes.len())),
    ];
    spans.extend(apply_patch_line_count_spans(total_added, total_removed));
    lines.push(Line::from(spans));
}

fn render_apply_patch_edit_body(changes: &[ApplyPatchDisplayChange]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let show_file_headers = changes.len() > 1;
    for (index, change) in changes.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(String::new()));
        }
        if show_file_headers {
            let mut spans = vec![
                Span::styled("  └ ", Style::default().fg(Color::DarkGray)),
                single_ansi_text_span(change.display_path(), Style::default()),
                Span::raw(" "),
            ];
            spans.extend(apply_patch_line_count_spans(change.added, change.removed));
            lines.push(Line::from(spans));
        }
        lines.extend(change.lines.iter().map(|line| {
            render_apply_patch_diff_line(change.highlight_path(), line, show_file_headers)
        }));
    }
    lines
}

fn apply_patch_line_count_spans(added: usize, removed: usize) -> Vec<Span<'static>> {
    vec![
        Span::raw("("),
        Span::styled(format!("+{added}"), Style::default().fg(DIFF_ADDED_FG)),
        Span::raw(" "),
        Span::styled(format!("-{removed}"), Style::default().fg(DIFF_REMOVED_FG)),
        Span::raw(")"),
    ]
}

fn render_apply_patch_diff_line(
    path: &str,
    line: &ApplyPatchDisplayLine,
    nested: bool,
) -> Line<'static> {
    let indent = if nested { "    " } else { "  " };
    match line.kind {
        ApplyPatchDisplayLineKind::Hunk => Line::from(Span::styled(
            format!("{indent}{}", line.text),
            Style::default().fg(Color::DarkGray),
        )),
        ApplyPatchDisplayLineKind::Context => {
            let mut spans = vec![Span::styled(
                format!("{indent} "),
                Style::default().fg(Color::DarkGray),
            )];
            spans.extend(apply_patch_highlighted_content_spans(
                path, &line.text, None,
            ));
            Line::from(spans)
        }
        ApplyPatchDisplayLineKind::Add => {
            let style = Style::default().bg(DIFF_ADDED_BG);
            let mut spans = vec![Span::styled(format!("{indent}+"), style.fg(DIFF_ADDED_FG))];
            spans.extend(apply_patch_highlighted_content_spans(
                path,
                &line.text,
                Some(style),
            ));
            Line::from(spans)
        }
        ApplyPatchDisplayLineKind::Delete => {
            let style = Style::default()
                .bg(DIFF_REMOVED_BG)
                .add_modifier(Modifier::DIM);
            let mut spans = vec![Span::styled(
                format!("{indent}-"),
                style.fg(DIFF_REMOVED_FG),
            )];
            spans.extend(apply_patch_highlighted_content_spans(
                path,
                &line.text,
                Some(style),
            ));
            Line::from(spans)
        }
    }
}

fn apply_patch_highlighted_content_spans(
    path: &str,
    text: &str,
    overlay: Option<Style>,
) -> Vec<Span<'static>> {
    let lang = path_extension(path);
    let mut spans = lang
        .map(|lang| highlight_code_line(lang, text).spans)
        .unwrap_or_else(|| ansi_text_spans(text, Style::default()));
    if let Some(overlay) = overlay {
        for span in &mut spans {
            span.style = span.style.patch(overlay);
        }
    }
    spans
}

fn path_extension(path: &str) -> Option<&str> {
    path.rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty())
}

fn render_command_tool_call(input: &str) -> Vec<Line<'static>> {
    let command = input.trim();
    if command.is_empty() {
        return Vec::new();
    }
    vec![Line::from(vec![
        Span::styled(
            "$",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(command.to_string(), Style::default().fg(TRANSCRIPT_TOOL_FG)),
    ])]
}

fn render_generic_tool_call(call: ToolCallView<'_>) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(single_ansi_text_span(
        call.name,
        Style::default().fg(Color::Magenta),
    ))];

    if call.input.is_empty() {
        return lines;
    }

    lines.extend(tool_input_lines(call.input));
    lines
}

fn tool_input_lines(input: &str) -> Vec<Line<'static>> {
    if is_patch_text(input) {
        patch_lines(input)
    } else {
        text_lines(input)
    }
}

fn structured_tool_output_display_lines(
    display: &ToolOutputDisplayRecord,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    match display {
        ToolOutputDisplayRecord::InspectRead(reads) => {
            let mut lines = Vec::new();
            for (index, read) in reads.iter().enumerate() {
                if index > 0 {
                    lines.push(Line::from(String::new()));
                }
                lines.extend(render_inspect_read_output(read));
            }
            bounded_display_lines(
                wrap_content_lines(lines, wrap_width),
                TOOL_BLOCK_LINE_BUDGET,
            )
        }
    }
}

fn render_inspect_read_output(read: &InspectReadDisplayRecord) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let end_line = read
        .start_line
        .saturating_add(read.lines.len())
        .saturating_sub(1);
    lines.push(render_inspect_read_result_header(read, end_line));
    let line_number_width = read
        .start_line
        .saturating_add(read.lines.len().saturating_sub(1))
        .to_string()
        .len()
        .max(1);
    for (index, text) in read.lines.iter().enumerate() {
        let line_number = read.start_line + index;
        lines.push(render_inspect_read_result_line(
            &read.path,
            line_number,
            line_number_width,
            text,
        ));
    }
    if let Some(next) = read.next {
        lines.push(Line::from(vec![
            Span::styled(
                "next",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{}+{}", next.start_line, next.line_count),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    lines
}

fn render_inspect_read_result_header(
    read: &InspectReadDisplayRecord,
    end_line: usize,
) -> Line<'static> {
    let range = if read.lines.is_empty() {
        format!(":{} no lines", read.start_line)
    } else {
        format!(":{}-{end_line}", read.start_line)
    };
    Line::from(vec![
        Span::styled(
            "Read",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(read.path.clone(), Style::default().fg(TRANSCRIPT_TOOL_FG)),
        Span::styled(range, Style::default().fg(Color::DarkGray)),
    ])
}

fn render_inspect_read_result_line(
    path: &str,
    line_number: usize,
    line_number_width: usize,
    text: &str,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{line_number:>line_number_width$} │ "),
        Style::default().fg(Color::DarkGray),
    )];
    let lang = path_extension(path);
    let mut content = lang
        .map(|lang| highlight_code_line(lang, text).spans)
        .unwrap_or_else(|| ansi_text_spans(text, Style::default()));
    spans.append(&mut content);
    Line::from(spans)
}

fn tool_output_lines(
    output: &str,
    failed_apply_patch_output: bool,
    successful_apply_patch_output: bool,
    terminal_context: Option<TerminalOutputContext>,
    terminal_echoed_input: Option<&str>,
) -> Vec<Line<'static>> {
    if successful_apply_patch_output {
        return Vec::new();
    }
    if terminal_context == Some(TerminalOutputContext::Hidden) {
        return Vec::new();
    }

    let mut lines = match terminal_context {
        Some(TerminalOutputContext::Display) => {
            terminal_display_output_lines(output, terminal_echoed_input)
        }
        None => tool_fallback_output_lines(output),
        Some(TerminalOutputContext::Hidden) => unreachable!("hidden terminal output returned"),
    };
    if failed_apply_patch_output {
        apply_tool_error_style(&mut lines);
    }
    lines
}

fn tool_output_display_lines(
    output: &str,
    failed_apply_patch_output: bool,
    successful_apply_patch_output: bool,
    terminal_context: Option<TerminalOutputContext>,
    terminal_echoed_input: Option<&str>,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    let wrapped = wrap_content_lines(
        tool_output_lines(
            output,
            failed_apply_patch_output,
            successful_apply_patch_output,
            terminal_context,
            terminal_echoed_input,
        ),
        wrap_width,
    );
    match terminal_context {
        Some(TerminalOutputContext::Hidden) => wrapped,
        Some(TerminalOutputContext::Display) | None => {
            bounded_display_lines(wrapped, MAX_TOOL_OUTPUT_DISPLAY_LINES)
        }
    }
}

fn terminal_display_output_lines(output: &str, echoed_input: Option<&str>) -> Vec<Line<'static>> {
    let body = parse_tool_output_envelope(output).map_or(output, |envelope| envelope.body);
    bounded_display_lines(
        terminal_display_body_lines(body, echoed_input),
        MAX_TOOL_OUTPUT_DISPLAY_LINES,
    )
}

fn tool_fallback_output_lines(output: &str) -> Vec<Line<'static>> {
    let body = parse_tool_output_envelope(output).map_or(output, |envelope| envelope.body);
    let lines = terminal_output_lines(display_tool_output_body(body).as_ref());
    bounded_display_lines(lines, MAX_TOOL_OUTPUT_DISPLAY_LINES)
}

fn terminal_display_body_lines(body: &str, echoed_input: Option<&str>) -> Vec<Line<'static>> {
    let mut lines = terminal_output_lines(display_tool_output_body(body).as_ref());
    if let Some(echoed_input) = echoed_input {
        strip_echoed_display_lines(&mut lines, echoed_input);
    }
    lines
}

fn strip_echoed_display_lines(lines: &mut Vec<Line<'static>>, echoed_input: &str) {
    let echoed_lines = echoed_input
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if echoed_lines.is_empty() || lines.len() < echoed_lines.len() {
        return;
    }
    let matches_echo = echoed_lines.iter().enumerate().all(|(index, echoed)| {
        let output_line = line_plain_text(&lines[index]);
        let output_line = output_line.trim_end();
        output_line == *echoed || output_line.trim_start().ends_with(*echoed)
    });
    if matches_echo {
        lines.drain(..echoed_lines.len());
    }
}

fn display_tool_output_body(body: &str) -> std::borrow::Cow<'_, str> {
    if !body.contains(TRANSCRIPT_TRUNCATED_MARKER.trim_end()) {
        return std::borrow::Cow::Borrowed(body);
    }
    std::borrow::Cow::Owned(
        body.lines()
            .filter(|line| *line != TRANSCRIPT_TRUNCATED_MARKER.trim_end())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[derive(Debug, Clone, Copy)]
struct FreeformToolInput<'a> {
    raw: &'a str,
}

impl<'a> FreeformToolInput<'a> {
    fn new(raw: &'a str) -> Self {
        Self { raw }
    }

    fn body(&self, key: &str) -> Option<String> {
        freeform_body_value(self.raw, key)
    }
}

fn terminal_tool_call_lines(
    kind: TerminalToolKind,
    input: FreeformToolInput<'_>,
) -> Vec<Line<'static>> {
    match kind {
        TerminalToolKind::Read => Vec::new(),
        TerminalToolKind::Open => terminal_command_tool_call_lines(input),
        TerminalToolKind::Write => input
            .body("input")
            .map_or_else(Vec::new, |stdin| terminal_action_line("stdin", stdin)),
    }
}

fn terminal_command_tool_call_lines(input: FreeformToolInput<'_>) -> Vec<Line<'static>> {
    input
        .body("command")
        .map_or_else(Vec::new, |command| terminal_action_line("$", command))
}

fn terminal_action_line(label: &str, value: String) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(value, Style::default().fg(TRANSCRIPT_TOOL_FG)),
    ])]
}

fn terminal_output_context_for_tool_call(
    kind: ToolCallKind,
    input: &str,
) -> Option<TerminalOutputContext> {
    let ToolCallKind::Terminal(kind) = kind else {
        return None;
    };
    match kind {
        TerminalToolKind::Read => Some(TerminalOutputContext::Hidden),
        TerminalToolKind::Write => Some(TerminalOutputContext::Display),
        TerminalToolKind::Open => {
            let command = FreeformToolInput::new(input).body("command")?;
            match terminal_command_intent(&command) {
                TerminalCommandIntent::FileRead | TerminalCommandIntent::Search => {
                    Some(TerminalOutputContext::Hidden)
                }
                TerminalCommandIntent::Other => Some(TerminalOutputContext::Display),
            }
        }
    }
}

fn terminal_submitted_input_for_tool_call(kind: ToolCallKind, input: &str) -> Option<String> {
    let ToolCallKind::Terminal(kind) = kind else {
        return None;
    };
    match kind {
        TerminalToolKind::Write => FreeformToolInput::new(input).body("input"),
        TerminalToolKind::Open | TerminalToolKind::Read => None,
    }
}

fn terminal_command_intent(command: &str) -> TerminalCommandIntent {
    let mut has_file_read = false;
    for line in command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let tokens = shell_words_for_display(line);
        if command_sequence_has_intent(&tokens, TerminalCommandIntent::Search) {
            return TerminalCommandIntent::Search;
        }
        has_file_read |= command_sequence_has_intent(&tokens, TerminalCommandIntent::FileRead);
    }
    if has_file_read {
        TerminalCommandIntent::FileRead
    } else {
        TerminalCommandIntent::Other
    }
}

fn command_sequence_has_intent(tokens: &[String], intent: TerminalCommandIntent) -> bool {
    let mut start = 0usize;
    for index in 0..=tokens.len() {
        if index != tokens.len() && !is_shell_sequence_separator(tokens[index].as_str()) {
            continue;
        }
        if start < index && command_tokens_intent(&tokens[start..index]) == intent {
            return true;
        }
        start = index + 1;
    }
    false
}

fn command_tokens_intent(tokens: &[String]) -> TerminalCommandIntent {
    if summarize_read_targets(tokens).is_some() {
        return TerminalCommandIntent::FileRead;
    }
    match tokens
        .first()
        .map(String::as_str)
        .map(command_basename)
        .unwrap_or_default()
    {
        "rg" => TerminalCommandIntent::Search,
        "head" | "tail" => TerminalCommandIntent::FileRead,
        _ => TerminalCommandIntent::Other,
    }
}

fn freeform_body_value(input: &str, key: &str) -> Option<String> {
    let mut lines = input.lines();
    while let Some(line) = lines.next() {
        let Some(value) = strip_freeform_key(line, key) else {
            continue;
        };
        if !value.is_empty() {
            return Some(value.to_string());
        }
        return Some(lines.collect::<Vec<_>>().join("\n"));
    }
    None
}

fn strip_freeform_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.trim_start().strip_prefix(key)?;
    let rest = rest.strip_prefix(':')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest).trim_end())
}

fn is_shell_sequence_separator(token: &str) -> bool {
    matches!(token, "&&" | ";" | "|")
}

fn command_basename(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

fn summarize_read_targets(tokens: &[String]) -> Option<Vec<String>> {
    match command_basename(tokens.first()?.as_str()) {
        "sed" => summarize_sed_read_targets(tokens),
        "cat" => {
            let paths = non_option_args(tokens, 1);
            (!paths.is_empty()).then(|| paths.into_iter().map(str::to_string).collect())
        }
        _ => None,
    }
}

fn summarize_sed_read_targets(tokens: &[String]) -> Option<Vec<String>> {
    let mut expression: Option<&str> = None;
    let mut paths = Vec::new();
    let mut index = 1usize;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        match token {
            "-n" => index += 1,
            "-e" | "--expression" => {
                expression = tokens.get(index + 1).map(String::as_str);
                index += 2;
            }
            "--" => {
                paths.extend(tokens[index + 1..].iter().map(String::as_str));
                break;
            }
            _ if token.starts_with("-e") && token.len() > 2 => {
                expression = Some(&token[2..]);
                index += 1;
            }
            _ if token.starts_with("--expression=") => {
                expression = token.strip_prefix("--expression=");
                index += 1;
            }
            _ if token.starts_with('-') => index += 1,
            _ => {
                if expression.is_none() && parse_sed_print_range(token).is_some() {
                    expression = Some(token);
                } else {
                    paths.push(token);
                }
                index += 1;
            }
        }
    }

    let range = parse_sed_print_range(expression?)?;
    (!paths.is_empty()).then(|| {
        paths
            .into_iter()
            .map(|path| format!("{path}:{range}"))
            .collect()
    })
}

fn parse_sed_print_range(expression: &str) -> Option<String> {
    let expression = expression.trim().strip_suffix('p')?.trim();
    if let Some((start, end)) = expression.split_once(',') {
        if is_line_number(start) && is_line_number(end) {
            return Some(format!("{start}-{end}"));
        }
    }
    if is_line_number(expression) {
        return Some(expression.to_string());
    }
    None
}

fn is_line_number(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
}

fn non_option_args(tokens: &[String], start: usize) -> Vec<&str> {
    let mut args = Vec::new();
    let mut index = start;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if token == "--" {
            args.extend(tokens[index + 1..].iter().map(String::as_str));
            break;
        }
        if !token.starts_with('-') {
            args.push(token);
        }
        index += 1;
    }
    args
}

fn shell_words_for_display(command_line: &str) -> Vec<String> {
    const MAX_SHELL_WORDS_FOR_DISPLAY: usize = 64;

    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = command_line.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                _ => current.push(ch),
            },
            _ => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                ';' | '|' => {
                    if push_shell_word(&mut words, &mut current, MAX_SHELL_WORDS_FOR_DISPLAY) {
                        return words;
                    }
                    words.push(ch.to_string());
                    if words.len() == MAX_SHELL_WORDS_FOR_DISPLAY {
                        return words;
                    }
                }
                '&' if chars.peek() == Some(&'&') => {
                    chars.next();
                    if push_shell_word(&mut words, &mut current, MAX_SHELL_WORDS_FOR_DISPLAY) {
                        return words;
                    }
                    words.push("&&".to_string());
                    if words.len() == MAX_SHELL_WORDS_FOR_DISPLAY {
                        return words;
                    }
                }
                ch if ch.is_whitespace() => {
                    if push_shell_word(&mut words, &mut current, MAX_SHELL_WORDS_FOR_DISPLAY) {
                        return words;
                    }
                }
                _ => current.push(ch),
            },
        }
    }
    if !current.is_empty() && words.len() < MAX_SHELL_WORDS_FOR_DISPLAY {
        words.push(current);
    }
    words
}

fn push_shell_word(words: &mut Vec<String>, current: &mut String, max_words: usize) -> bool {
    if !current.is_empty() {
        words.push(std::mem::take(current));
    }
    words.len() == max_words
}

fn parse_tool_output_envelope(output: &str) -> Option<ToolOutputEnvelopeView<'_>> {
    let mut status = None;
    let mut token_count = None;
    let mut saw_envelope_line = false;
    let mut saw_unknown_prelude = false;
    let mut body_start = None;
    let mut offset = 0usize;

    for segment in output.split_inclusive('\n') {
        let line = segment.trim_end_matches('\n').trim_end_matches('\r');
        let next_offset = offset + segment.len();
        if line == "Output:" {
            body_start = Some(next_offset);
            break;
        }
        if line.starts_with("Chunk ID:") || line.starts_with("Wall time:") {
            saw_envelope_line = true;
        } else if let Some(code) = line.strip_prefix("Process exited with code ") {
            status = Some(format!("exit {}", code.trim()));
            saw_envelope_line = true;
        } else if let Some(terminal_id) = line.strip_prefix("Terminal running with ID ") {
            status = Some(format!("running terminal {}", terminal_id.trim()));
            saw_envelope_line = true;
        } else if let Some(count) = line.strip_prefix("Original token count:") {
            token_count = Some(count.trim());
            saw_envelope_line = true;
        } else if !line.is_empty() {
            saw_unknown_prelude = true;
        }
        offset = next_offset;
    }

    if !saw_envelope_line || saw_unknown_prelude {
        return None;
    }

    Some(ToolOutputEnvelopeView {
        status,
        token_count,
        body: body_start.map_or("", |start| &output[start..]),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnsiStyleState {
    fg: Option<Color>,
    bg: Option<Color>,
    modifiers: Modifier,
}

impl AnsiStyleState {
    fn style(self) -> Style {
        let mut style = Style::default();
        if let Some(fg) = self.fg {
            style = style.fg(fg);
        }
        if let Some(bg) = self.bg {
            style = style.bg(bg);
        }
        style.add_modifier(self.modifiers)
    }
}

impl Default for AnsiStyleState {
    fn default() -> Self {
        Self {
            fg: None,
            bg: None,
            modifiers: Modifier::empty(),
        }
    }
}

fn terminal_output_lines(input: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut segment = String::new();
    let mut style = AnsiStyleState::default();
    let mut index = 0usize;
    let bytes = input.as_bytes();

    while index < bytes.len() {
        if bytes[index] == 0x1b {
            push_terminal_segment(&mut spans, &mut segment, style);
            consume_ansi_escape(bytes, &mut index, &mut style);
            skip_to_char_boundary(input, &mut index);
            continue;
        }

        let character = input[index..]
            .chars()
            .next()
            .expect("index is always at a char boundary");
        index += character.len_utf8();
        match character {
            '\r' if bytes.get(index) == Some(&b'\n') => {
                index += 1;
                push_terminal_line(&mut lines, &mut spans, &mut segment, style);
            }
            '\n' => push_terminal_line(&mut lines, &mut spans, &mut segment, style),
            '\t' => segment.push_str("    "),
            character if character.is_control() => {}
            character => segment.push(character),
        }
    }

    push_terminal_segment(&mut spans, &mut segment, style);
    if !spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

fn display_text(input: &str) -> String {
    ansi_text_spans(input, Style::default())
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect()
}

fn single_ansi_text_span(text: impl AsRef<str>, style: Style) -> Span<'static> {
    Span::styled(display_text(text.as_ref()), style)
}

fn ansi_text_spans(input: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut segment = String::new();
    let mut ansi_style = AnsiStyleState::default();
    let mut index = 0usize;
    let bytes = input.as_bytes();

    while index < bytes.len() {
        if bytes[index] == 0x1b {
            push_ansi_text_segment(&mut spans, &mut segment, base_style, ansi_style);
            consume_ansi_escape(bytes, &mut index, &mut ansi_style);
            skip_to_char_boundary(input, &mut index);
            continue;
        }

        let character = input[index..]
            .chars()
            .next()
            .expect("index is always at a char boundary");
        index += character.len_utf8();
        match character {
            '\t' => segment.push_str("    "),
            character if character.is_control() => {}
            character => segment.push(character),
        }
    }

    push_ansi_text_segment(&mut spans, &mut segment, base_style, ansi_style);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }
    spans
}

fn push_ansi_text_segment(
    spans: &mut Vec<Span<'static>>,
    segment: &mut String,
    base_style: Style,
    ansi_style: AnsiStyleState,
) {
    if !segment.is_empty() {
        spans.push(Span::styled(
            std::mem::take(segment),
            base_style.patch(ansi_style.style()),
        ));
    }
}

fn push_terminal_line(
    lines: &mut Vec<Line<'static>>,
    spans: &mut Vec<Span<'static>>,
    segment: &mut String,
    style: AnsiStyleState,
) {
    push_terminal_segment(spans, segment, style);
    lines.push(Line::from(std::mem::take(spans)));
}

fn push_terminal_segment(
    spans: &mut Vec<Span<'static>>,
    segment: &mut String,
    style: AnsiStyleState,
) {
    if !segment.is_empty() {
        spans.push(Span::styled(std::mem::take(segment), style.style()));
    }
}

fn consume_ansi_escape(bytes: &[u8], index: &mut usize, style: &mut AnsiStyleState) {
    *index += 1;
    let Some(marker) = bytes.get(*index).copied() else {
        return;
    };
    match marker {
        b'[' => consume_csi(bytes, index, style),
        b']' => consume_string_escape(bytes, index, true),
        b'P' | b'^' | b'_' => consume_string_escape(bytes, index, false),
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => {
            consume_ansi_intermediate_and_final_byte(bytes, index);
        }
        0x20..=0x7e => {
            *index += 1;
        }
        _ => {}
    }
}

fn skip_to_char_boundary(input: &str, index: &mut usize) {
    while *index < input.len() && !input.is_char_boundary(*index) {
        *index += 1;
    }
}

fn consume_ansi_intermediate_and_final_byte(bytes: &[u8], index: &mut usize) {
    *index += 1;
    if matches!(bytes.get(*index), Some(0x20..=0x7e)) {
        *index += 1;
    }
}

fn consume_csi(bytes: &[u8], index: &mut usize, style: &mut AnsiStyleState) {
    *index += 1;
    let params_start = *index;
    while *index < bytes.len() {
        let byte = bytes[*index];
        *index += 1;
        if (0x40..=0x7e).contains(&byte) {
            if byte == b'm' {
                apply_sgr_params(&bytes[params_start..*index - 1], style);
            }
            break;
        }
    }
}

fn consume_string_escape(bytes: &[u8], index: &mut usize, allow_bel: bool) {
    *index += 1;
    while *index < bytes.len() {
        if allow_bel && bytes[*index] == 0x07 {
            *index += 1;
            break;
        }
        if bytes[*index] == 0x1b && *index + 1 < bytes.len() && bytes[*index + 1] == b'\\' {
            *index += 2;
            break;
        }
        *index += 1;
    }
}

fn apply_sgr_params(params: &[u8], style: &mut AnsiStyleState) {
    if params.is_empty() {
        *style = AnsiStyleState::default();
        return;
    }
    let params = String::from_utf8_lossy(params);
    let codes = params
        .split(';')
        .map(|part| part.trim().parse::<u16>().ok())
        .collect::<Vec<_>>();
    if codes.is_empty() || codes == [Some(0)] {
        *style = AnsiStyleState::default();
        return;
    }
    if codes.iter().all(Option::is_none) {
        return;
    }

    let mut index = 0usize;
    while index < codes.len() {
        let Some(code) = codes[index] else {
            index += 1;
            continue;
        };
        match code {
            0 => *style = AnsiStyleState::default(),
            30..=37 => style.fg = Some(ansi_basic_color((code - 30) as u8, false)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(ansi_basic_color((code - 40) as u8, false)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(ansi_basic_color((code - 90) as u8, true)),
            100..=107 => style.bg = Some(ansi_basic_color((code - 100) as u8, true)),
            38 | 48 => {
                let target_fg = code == 38;
                if let Some((color, consumed)) = parse_extended_ansi_color(&codes[index + 1..]) {
                    if target_fg {
                        style.fg = Some(color);
                    } else {
                        style.bg = Some(color);
                    }
                    index += consumed;
                }
            }
            _ => {}
        }
        index += 1;
    }
}

fn parse_extended_ansi_color(codes: &[Option<u16>]) -> Option<(Color, usize)> {
    match codes {
        [Some(5), Some(index), ..] => Some((Color::Indexed((*index).min(255) as u8), 2)),
        [Some(2), Some(red), Some(green), Some(blue), ..] => Some((
            Color::Rgb(
                (*red).min(255) as u8,
                (*green).min(255) as u8,
                (*blue).min(255) as u8,
            ),
            4,
        )),
        _ => None,
    }
}

fn ansi_basic_color(index: u8, bright: bool) -> Color {
    match (index, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        (7, true) => Color::White,
        _ => Color::Gray,
    }
}

fn apply_failed_patch_style(lines: &mut [Line<'static>]) {
    for line in lines {
        for span in &mut line.spans {
            span.style = span
                .style
                .fg
                .map_or(Style::default().fg(TRANSCRIPT_FAILED_PATCH_FG), |_| {
                    span.style.add_modifier(Modifier::DIM)
                });
        }
    }
}

fn apply_tool_error_style(lines: &mut [Line<'static>]) {
    for line in lines {
        for span in &mut line.spans {
            span.style = span.style.fg(TRANSCRIPT_TOOL_ERROR_FG);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptKind {
    Assistant,
    Developer,
    User,
    Tool,
    Error,
    Event,
}

impl TranscriptKind {
    fn marker(self) -> &'static str {
        match self {
            Self::Assistant => TRANSCRIPT_ASSISTANT_MARKER,
            Self::Developer | Self::User => TRANSCRIPT_INPUT_MARKER,
            Self::Tool => TRANSCRIPT_TOOL_MARKER,
            Self::Error => TRANSCRIPT_ERROR_MARKER,
            Self::Event => TRANSCRIPT_EVENT_MARKER,
        }
    }

    fn accent(self) -> Color {
        match self {
            Self::Assistant => TRANSCRIPT_ASSISTANT_FG,
            Self::Developer => TRANSCRIPT_DEVELOPER_FG,
            Self::User => TRANSCRIPT_USER_FG,
            Self::Tool => TRANSCRIPT_TOOL_FG,
            Self::Error => Color::Red,
            Self::Event => TRANSCRIPT_EVENT_FG,
        }
    }

    fn body_color(self) -> Option<Color> {
        match self {
            Self::Assistant => Some(TRANSCRIPT_ASSISTANT_TEXT_FG),
            Self::Developer | Self::User => Some(TRANSCRIPT_INPUT_TEXT_FG),
            Self::Tool | Self::Error | Self::Event => None,
        }
    }
}

fn classify_transcript_entry(entry: &str) -> (TranscriptKind, &str) {
    if let Some(text) = strip_transcript_prefix(entry, "assistant:") {
        return (TranscriptKind::Assistant, text);
    }
    if let Some(text) = strip_transcript_prefix(entry, "assistant>") {
        return (TranscriptKind::Assistant, text);
    }
    if let Some(text) = strip_transcript_prefix(entry, "developer>") {
        return (TranscriptKind::Developer, text);
    }
    if let Some(text) = strip_transcript_prefix(entry, "user>") {
        return (TranscriptKind::User, text);
    }
    if let Some(text) = strip_transcript_prefix(entry, ">") {
        return (TranscriptKind::User, text);
    }
    if entry.starts_with("error:") || entry.starts_with("responses actor error:") {
        return (TranscriptKind::Error, entry);
    }
    (TranscriptKind::Event, entry)
}

fn strip_transcript_prefix<'a>(entry: &'a str, prefix: &str) -> Option<&'a str> {
    entry
        .strip_prefix(prefix)
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
}

fn decorate_transcript_lines(
    kind: TranscriptKind,
    lines: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| decorate_transcript_line(kind, index == 0, line))
        .collect()
}

pub(crate) fn transcript_content_width(viewport_width: u16) -> usize {
    usize::from(viewport_width).saturating_sub(3).max(1)
}

pub(crate) fn transcript_content_width_with_scrollbar(viewport_width: u16) -> usize {
    usize::from(viewport_width).saturating_sub(4).max(1)
}

fn wrap_content_lines(lines: Vec<Line<'static>>, wrap_width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| hard_wrap_line(line, wrap_width))
        .collect()
}

fn hard_wrap_line(line: Line<'static>, wrap_width: usize) -> Vec<Line<'static>> {
    let wrap_width = wrap_width.max(1);
    let mut output = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;
    for span in line.spans {
        let mut segment = String::new();
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = grapheme.width();
            if current_width > 0 && current_width.saturating_add(grapheme_width) > wrap_width {
                if !segment.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut segment), span.style));
                }
                output.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }
            segment.push_str(grapheme);
            current_width = current_width.saturating_add(grapheme_width);
            if current_width >= wrap_width {
                current.push(Span::styled(std::mem::take(&mut segment), span.style));
                output.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }
        }
        if !segment.is_empty() {
            current.push(Span::styled(segment, span.style));
        }
    }
    if !current.is_empty() || output.is_empty() {
        output.push(Line::from(current));
    }
    output
}

fn hard_wrap_plain_lines(lines: Vec<String>, wrap_width: usize) -> Vec<String> {
    lines
        .into_iter()
        .flat_map(|line| hard_wrap_plain_line(&line, wrap_width))
        .collect()
}

fn hard_wrap_plain_line(line: &str, wrap_width: usize) -> Vec<String> {
    let wrap_width = wrap_width.max(1);
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut output = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for grapheme in line.graphemes(true) {
        let grapheme_width = grapheme.width();
        if current_width > 0 && current_width.saturating_add(grapheme_width) > wrap_width {
            output.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width = current_width.saturating_add(grapheme_width);
        if current_width >= wrap_width {
            output.push(std::mem::take(&mut current));
            current_width = 0;
        }
    }
    if !current.is_empty() {
        output.push(current);
    }
    output
}

fn decorate_transcript_line(
    kind: TranscriptKind,
    first_line: bool,
    mut line: Line<'static>,
) -> Line<'static> {
    let accent = kind.accent();
    let marker = if first_line { kind.marker() } else { " " };
    apply_transcript_body_style(kind, &mut line);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(marker, Style::default().fg(accent)),
        Span::raw(" "),
    ];
    spans.extend(line.spans);
    Line::from(spans)
}

fn apply_transcript_body_style(kind: TranscriptKind, line: &mut Line<'static>) {
    let Some(text_color) = kind.body_color() else {
        return;
    };
    for span in &mut line.spans {
        if span.style.fg.is_none() {
            span.style = span.style.fg(text_color);
        }
    }
}

fn append_with_budget(
    target: &mut Vec<Line<'static>>,
    mut source: Vec<Line<'static>>,
    budget: usize,
) {
    if target.len() + source.len() <= budget {
        target.append(&mut source);
        return;
    }

    let remaining = budget.saturating_sub(target.len()).saturating_sub(1);
    let omitted = source.len().saturating_sub(remaining);
    target.extend(source.into_iter().take(remaining));
    target.push(omitted_lines_line(omitted));
}

fn append_with_head_tail_budget(
    target: &mut Vec<Line<'static>>,
    source: Vec<Line<'static>>,
    budget: usize,
) {
    if target.len() + source.len() <= budget {
        target.extend(source);
        return;
    }

    let available = budget.saturating_sub(target.len());
    if available == 0 {
        return;
    }
    if available == 1 {
        target.push(omitted_lines_line(source.len()));
        return;
    }

    let content_slots = available - 1;
    let head_count = content_slots.div_ceil(2);
    let tail_count = content_slots - head_count;
    let omitted = source
        .len()
        .saturating_sub(head_count)
        .saturating_sub(tail_count);
    target.extend(source.iter().take(head_count).cloned());
    target.push(omitted_lines_line(omitted));
    target.extend(
        source
            .iter()
            .skip(source.len().saturating_sub(tail_count))
            .cloned(),
    );
}

fn bounded_display_lines(source: Vec<Line<'static>>, budget: usize) -> Vec<Line<'static>> {
    if source.len() <= budget {
        return source;
    }
    if budget == 0 {
        return Vec::new();
    }
    if budget == 1 {
        return vec![more_lines_line(source.len())];
    }
    let visible = budget - 1;
    let omitted = source.len().saturating_sub(visible);
    source
        .into_iter()
        .take(visible)
        .chain(std::iter::once(more_lines_line(omitted)))
        .collect()
}

fn omitted_lines_line(omitted: usize) -> Line<'static> {
    more_lines_line(omitted)
}

fn more_lines_line(omitted: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("and {omitted} more lines"),
        Style::default().fg(Color::DarkGray),
    ))
}

fn is_patch_text(input: &str) -> bool {
    input.starts_with("*** Begin Patch") || input.contains("\n*** Update File:")
}

fn patch_lines(input: &str) -> Vec<Line<'static>> {
    input
        .lines()
        .map(|line| {
            let style = if line.starts_with("*** Update File:")
                || line.starts_with("*** Add File:")
                || line.starts_with("*** Delete File:")
            {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with("@@") {
                Style::default().fg(Color::Cyan)
            } else if line.starts_with('+') {
                Style::default().fg(Color::Green)
            } else if line.starts_with('-') {
                Style::default().fg(Color::Red)
            } else if line.starts_with("***") {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled("  ", style)];
            spans.extend(ansi_text_spans(line, style));
            Line::from(spans)
        })
        .collect()
}

fn text_lines(input: &str) -> Vec<Line<'static>> {
    text_lines_range(input, 0, text_line_count(input))
}

fn text_lines_with_options(input: &str, options: TextRenderOptions) -> Vec<Line<'static>> {
    text_lines_range_with_options(input, 0, text_line_count(input), options)
}

fn text_line_count(input: &str) -> usize {
    input.lines().count().max(1)
}

fn text_content_lines(input: &str) -> Vec<String> {
    let lines = input.lines().map(display_text).collect::<Vec<_>>();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextRenderOptions {
    syntax_highlighting: bool,
}

impl TextRenderOptions {
    const HIGHLIGHTED: Self = Self {
        syntax_highlighting: true,
    };
    const PLAIN: Self = Self {
        syntax_highlighting: false,
    };
}

fn text_lines_range(input: &str, start: usize, take: usize) -> Vec<Line<'static>> {
    text_lines_range_with_options(input, start, take, TextRenderOptions::HIGHLIGHTED)
}

fn text_lines_range_with_options(
    input: &str,
    start: usize,
    take: usize,
    options: TextRenderOptions,
) -> Vec<Line<'static>> {
    if take == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut code_lang: Option<String> = None;
    let mut emitted = 0usize;
    let raw_lines = input.lines().collect::<Vec<_>>();
    if raw_lines.is_empty() {
        if start == 0 {
            lines.push(Line::from(String::new()));
        }
        return lines;
    }
    for (index, raw_line) in raw_lines.into_iter().enumerate() {
        if let Some(lang) = raw_line.strip_prefix("```") {
            let lang = lang.trim();
            code_lang = if code_lang.is_some() {
                None
            } else if lang.is_empty() {
                Some("text".to_string())
            } else {
                Some(lang.to_ascii_lowercase())
            };
            if index >= start {
                lines.push(Line::from(ansi_text_spans(
                    raw_line,
                    Style::default().fg(Color::DarkGray),
                )));
                emitted += 1;
                if emitted == take {
                    break;
                }
            }
            continue;
        }

        if index >= start {
            if let Some(lang) = code_lang.as_deref() {
                let line = if options.syntax_highlighting {
                    highlight_code_line(lang, raw_line)
                } else {
                    plain_code_line(raw_line)
                };
                lines.push(line);
            } else {
                lines.push(markdown_text_line(raw_line));
            }
            emitted += 1;
            if emitted == take {
                break;
            }
        }
    }
    lines
}

fn plain_code_line(line: &str) -> Line<'static> {
    Line::from(ansi_text_spans(line, Style::default().fg(Color::Gray)))
}

fn markdown_text_line(line: &str) -> Line<'static> {
    let mut output = markdown_inline_spans(line);
    if is_markdown_heading_line(line) {
        for span in &mut output {
            span.style = span.style.add_modifier(Modifier::BOLD);
        }
    }
    Line::from(output)
}

fn markdown_inline_spans(line: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = line;
    loop {
        match (rest.find('`'), rest.find("**")) {
            (None, None) => {
                push_raw_span(&mut spans, rest);
                break;
            }
            (Some(code_open), Some(bold_open)) if code_open < bold_open => {
                let Some(next_rest) = push_inline_code_markup(&mut spans, rest, code_open) else {
                    break;
                };
                rest = next_rest;
            }
            (Some(code_open), None) => {
                let Some(next_rest) = push_inline_code_markup(&mut spans, rest, code_open) else {
                    break;
                };
                rest = next_rest;
            }
            (_, Some(bold_open)) => {
                let Some(next_rest) = push_bold_markup(&mut spans, rest, bold_open) else {
                    break;
                };
                rest = next_rest;
            }
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

fn push_inline_code_markup<'a>(
    spans: &mut Vec<Span<'static>>,
    rest: &'a str,
    open: usize,
) -> Option<&'a str> {
    push_raw_span(spans, &rest[..open]);
    let after_open = &rest[open + 1..];
    let Some(close) = after_open.find('`') else {
        push_raw_span(spans, &rest[open..]);
        return None;
    };
    push_raw_span(spans, "`");
    push_styled_span(
        spans,
        &after_open[..close],
        Style::default().fg(TRANSCRIPT_INLINE_CODE_FG),
    );
    push_raw_span(spans, "`");
    Some(&after_open[close + 1..])
}

fn push_bold_markup<'a>(
    spans: &mut Vec<Span<'static>>,
    rest: &'a str,
    open: usize,
) -> Option<&'a str> {
    push_raw_span(spans, &rest[..open]);
    let after_open = &rest[open + 2..];
    let Some(close) = after_open.find("**") else {
        push_raw_span(spans, &rest[open..]);
        return None;
    };
    push_raw_span(spans, "**");
    push_styled_span(
        spans,
        &after_open[..close],
        Style::default().add_modifier(Modifier::BOLD),
    );
    push_raw_span(spans, "**");
    Some(&after_open[close + 2..])
}

fn push_raw_span(spans: &mut Vec<Span<'static>>, text: &str) {
    push_styled_span(spans, text, Style::default());
}

fn push_styled_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if !text.is_empty() {
        spans.extend(ansi_text_spans(text, style));
    }
}

fn is_markdown_heading_line(line: &str) -> bool {
    let marker_count = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    (1..=6).contains(&marker_count)
        && line[marker_count..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
}

static SYNTECT_SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static SYNTECT_THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);
static SYNTAX_HIGHLIGHT_CACHE: LazyLock<Mutex<SyntaxHighlightCache>> =
    LazyLock::new(|| Mutex::new(SyntaxHighlightCache::default()));

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SyntaxHighlightCacheKey {
    lang: String,
    line: String,
}

impl SyntaxHighlightCacheKey {
    fn new(lang: &str, line: &str) -> Self {
        Self {
            lang: lang.trim_start_matches('.').to_ascii_lowercase(),
            line: line.to_string(),
        }
    }
}

#[derive(Debug, Default)]
struct SyntaxHighlightCache {
    entries: HashMap<SyntaxHighlightCacheKey, Line<'static>>,
    insertion_order: VecDeque<SyntaxHighlightCacheKey>,
}

impl SyntaxHighlightCache {
    fn get(&self, key: &SyntaxHighlightCacheKey) -> Option<Line<'static>> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: SyntaxHighlightCacheKey, line: Line<'static>) {
        if self.entries.contains_key(&key) {
            return;
        }
        self.entries.insert(key.clone(), line);
        self.insertion_order.push_back(key);
        while self.entries.len() > MAX_SYNTAX_HIGHLIGHT_CACHE_ENTRIES {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

fn highlight_code_line(lang: &str, line: &str) -> Line<'static> {
    if line.len() > MAX_SYNTAX_HIGHLIGHT_LINE_BYTES {
        return plain_code_line(line);
    }

    let key = SyntaxHighlightCacheKey::new(lang, line);
    if let Some(line) = SYNTAX_HIGHLIGHT_CACHE.lock().unwrap().get(&key) {
        return line;
    }
    let rendered = highlight_code_line_uncached(lang, line);
    SYNTAX_HIGHLIGHT_CACHE
        .lock()
        .unwrap()
        .insert(key, rendered.clone());
    rendered
}

fn highlight_code_line_uncached(lang: &str, line: &str) -> Line<'static> {
    if line.len() > MAX_SYNTAX_HIGHLIGHT_LINE_BYTES {
        return plain_code_line(line);
    }
    let syntax_set = &*SYNTECT_SYNTAX_SET;
    let Some(syntax) = syntect_syntax_for_lang(syntax_set, lang) else {
        return plain_code_line(line);
    };
    let theme = syntect_theme();
    let mut highlighter = HighlightLines::new(syntax, theme);
    let Ok(ranges) = highlighter.highlight_line(line, syntax_set) else {
        return plain_code_line(line);
    };
    Line::from(
        ranges
            .into_iter()
            .flat_map(|(style, text)| ansi_text_spans(text, ratatui_style_from_syntect(style)))
            .collect::<Vec<_>>(),
    )
}

fn syntect_syntax_for_lang<'a>(
    syntax_set: &'a SyntaxSet,
    lang: &str,
) -> Option<&'a SyntaxReference> {
    let lang = lang.trim_start_matches('.').to_ascii_lowercase();
    syntax_set
        .find_syntax_by_token(&lang)
        .or_else(|| syntax_set.find_syntax_by_extension(&lang))
}

fn syntect_theme() -> &'static Theme {
    SYNTECT_THEME_SET
        .themes
        .get("base16-ocean.dark")
        .expect("syntect default themes should include base16-ocean.dark")
}

fn ratatui_style_from_syntect(style: SyntectStyle) -> Style {
    Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ))
}

#[cfg(test)]
mod tests {
    use harness_core::sessions::TranscriptPage;

    use super::*;
    use crate::input::clamp_input_cursor;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn key_with_kind(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        modified_key_with_kind(code, KeyModifiers::NONE, kind)
    }

    fn modified_key_with_kind(
        code: KeyCode,
        modifiers: KeyModifiers,
        kind: KeyEventKind,
    ) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }
    fn text_entry_content(entry: &UiTranscriptEntry) -> &str {
        match entry {
            UiTranscriptEntry::Text(text) => text,
            UiTranscriptEntry::SessionRecord(_) => panic!("expected text transcript entry"),
        }
    }
    fn session_user_entry(text: &str) -> UiTranscriptEntry {
        UiTranscriptEntry::SessionRecord(SessionRecordKind::UserMessage(MessageRecord {
            text: text.to_string(),
        }))
    }

    fn session_assistant_entry(text: &str) -> UiTranscriptEntry {
        UiTranscriptEntry::SessionRecord(SessionRecordKind::AssistantMessage(MessageRecord {
            text: text.to_string(),
        }))
    }

    fn transcript_page_line(
        seq: u64,
        entry: UiTranscriptEntry,
    ) -> harness_core::sessions::TranscriptPageLine {
        let UiTranscriptEntry::SessionRecord(kind) = entry else {
            panic!("transcript page lines must be session records");
        };
        harness_core::sessions::TranscriptPageLine { seq, kind }
    }

    #[test]
    fn tui_app_edits_and_submits_input() {
        let mut app = TuiApp::new(UiSnapshot {
            session_id: "test-session".to_string(),
            thread_title: "thread".to_string(),
            provider: None,
            model_settings: Default::default(),
            developer_mode: true,
            response_streaming: false,
            last_ttft_ms: None,
            transcript_entries: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            queued_steering_prompt: None,
            agents: Vec::new(),
            ..Default::default()
        });

        assert_eq!(app.handle_key(key(KeyCode::Char('h'))), None);
        assert_eq!(app.handle_key(key(KeyCode::Char('i'))), None);
        let action = app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.snapshot().input, "");
        assert_eq!(action, Some(TuiAction::SubmitInput("hi".to_string())));
    }

    #[test]
    fn tui_app_queues_entered_input_while_streaming() {
        let mut app = TuiApp::new(UiSnapshot {
            response_streaming: true,
            input: " steer later ".to_string(),
            input_cursor: " steer later ".len(),
            ..Default::default()
        });

        let action = app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.snapshot().input, "");
        assert_eq!(
            action,
            Some(TuiAction::QueueSteering(" steer later ".to_string()))
        );
    }

    #[test]
    fn tui_app_submits_slash_command_while_streaming() {
        let mut app = TuiApp::new(UiSnapshot {
            response_streaming: true,
            input: " /model ".to_string(),
            input_cursor: " /model ".len(),
            ..Default::default()
        });

        let action = app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.snapshot().input, "");
        assert_eq!(action, Some(TuiAction::SubmitInput(" /model ".to_string())));
    }

    #[test]
    fn local_queued_steering_appends_without_transcript_line() {
        let mut app = TuiApp::new(UiSnapshot {
            response_streaming: true,
            queued_steering_prompt: Some("first".to_string()),
            ..Default::default()
        });
        let mut clipboard = Vec::new();

        apply_local_action(
            &mut app,
            &mut clipboard,
            TuiAction::QueueSteering(" second ".to_string()),
        )
        .unwrap();

        assert_eq!(
            app.snapshot().queued_steering_prompt,
            Some("first\nsecond".to_string())
        );
        assert_eq!(
            app.snapshot().transcript_entries,
            Vec::<UiTranscriptEntry>::new()
        );
    }

    #[test]
    fn runtime_queue_preview_appends_without_transcript_line() {
        let mut app = TuiApp::new(UiSnapshot {
            response_streaming: true,
            queued_steering_prompt: Some("first".to_string()),
            ..Default::default()
        });

        apply_runtime_action_preview(&mut app, &TuiAction::QueueSteering(" second ".to_string()));

        assert_eq!(
            app.snapshot().queued_steering_prompt,
            Some("first\nsecond".to_string())
        );
        assert_eq!(
            app.snapshot().transcript_entries,
            Vec::<UiTranscriptEntry>::new()
        );
    }

    #[test]
    fn stale_runtime_queue_ack_does_not_replace_local_append() {
        let mut app = TuiApp::new(UiSnapshot {
            queued_steering_prompt: Some("first\nsecond".to_string()),
            ..Default::default()
        });

        app.apply_runtime_event(RuntimeEvent::SteeringQueued(Some("first".to_string())));

        assert_eq!(
            app.snapshot().queued_steering_prompt,
            Some("first\nsecond".to_string())
        );
    }

    #[test]
    fn runtime_interrupt_preview_clears_queued_state_without_transcript_line() {
        let mut app = TuiApp::new(UiSnapshot {
            queued_steering_prompt: Some("first".to_string()),
            ..Default::default()
        });

        apply_runtime_action_preview(
            &mut app,
            &TuiAction::ApplySteering {
                text: "first".to_string(),
                mode: SteeringMode::InterruptNow,
            },
        );

        assert_eq!(app.snapshot().queued_steering_prompt, None);
        assert_eq!(
            app.snapshot().transcript_entries,
            Vec::<UiTranscriptEntry>::new()
        );
    }

    #[test]
    fn tui_app_paste_inserts_multiline_input_without_submitting() {
        let mut app = TuiApp::new(UiSnapshot::default());

        app.handle_paste("one\r\ntwo\rthree");

        assert_eq!(app.snapshot().input, "one\ntwo\nthree");
        assert_eq!(app.snapshot().input_cursor, "one\ntwo\nthree".len());
        assert_eq!(app.should_quit(), false);
    }

    #[test]
    fn tui_app_inserts_shifted_text_without_us_layout_guessing() {
        let mut app = TuiApp::new(UiSnapshot::default());

        let _ = app.handle_key(modified_key(KeyCode::Char('a'), KeyModifiers::SHIFT));
        let _ = app.handle_key(modified_key(KeyCode::Char('!'), KeyModifiers::SHIFT));
        let _ = app.handle_key(modified_key(KeyCode::Char('1'), KeyModifiers::SHIFT));

        assert_eq!(app.snapshot().input, "A!1");
        assert_eq!(app.snapshot().input_cursor, "A!1".len());
    }

    #[test]
    fn tui_app_handles_backspace_and_idle_escape_without_quit() {
        let mut app = TuiApp::new(UiSnapshot::default());

        let _ = app.handle_key(key(KeyCode::Char('a')));
        let _ = app.handle_key(key(KeyCode::Backspace));
        let _ = app.handle_key(key(KeyCode::Esc));

        assert_eq!(app.snapshot().input, "");
        assert_eq!(app.should_quit(), false);
    }

    #[test]
    fn tui_app_esc_applies_queued_steering_without_quit() {
        let mut app = TuiApp::new(UiSnapshot {
            queued_steering_prompt: Some("prefer the fast path".to_string()),
            ..Default::default()
        });

        let action = app.handle_key(key(KeyCode::Esc));

        assert_eq!(
            action,
            Some(TuiAction::ApplySteering {
                text: "prefer the fast path".to_string(),
                mode: SteeringMode::InterruptNow,
            })
        );
        assert_eq!(app.snapshot().queued_steering_prompt, None);
        assert_eq!(app.should_quit(), false);
    }

    #[test]
    fn tui_app_esc_interrupts_stream_with_current_input() {
        let mut app = TuiApp::new(UiSnapshot {
            response_streaming: true,
            input: " steer now ".to_string(),
            input_cursor: " steer now ".len(),
            ..Default::default()
        });

        let action = app.handle_key(key(KeyCode::Esc));

        assert_eq!(
            action,
            Some(TuiAction::ApplySteering {
                text: "steer now".to_string(),
                mode: SteeringMode::InterruptNow,
            })
        );
        assert_eq!(app.snapshot().input, "");
        assert_eq!(app.should_quit(), false);
    }

    #[test]
    fn tui_app_respects_terminal_backspace_repeat_events() {
        let mut app = TuiApp::new(UiSnapshot::default());

        for ch in "abc".chars() {
            let _ = app.handle_key(key(KeyCode::Char(ch)));
        }
        let _ = app.handle_key(key(KeyCode::Backspace));
        let _ = app.handle_key(key_with_kind(KeyCode::Backspace, KeyEventKind::Repeat));
        let _ = app.handle_key(key_with_kind(KeyCode::Backspace, KeyEventKind::Repeat));

        assert_eq!(app.snapshot().input, "");
    }

    #[test]
    fn tui_app_respects_terminal_shift_enter_repeat_events() {
        let mut app = TuiApp::new(UiSnapshot::default());

        let _ = app.handle_key(modified_key(KeyCode::Enter, KeyModifiers::SHIFT));
        let _ = app.handle_key(modified_key_with_kind(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
            KeyEventKind::Repeat,
        ));

        assert_eq!(app.snapshot().input, "\n\n");
        assert_eq!(app.snapshot().input_cursor, 2);
    }

    #[test]
    fn tui_app_ctrl_c_clears_input_then_requires_second_ctrl_c_to_quit() {
        let mut app = TuiApp::new(UiSnapshot::default());

        let _ = app.handle_key(key(KeyCode::Char('a')));
        let _ = app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(app.snapshot().input, "");
        assert_eq!(app.should_quit(), false);

        let _ = app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.should_quit(), false);
        assert_eq!(
            app.snapshot().transcript_entries,
            Vec::<UiTranscriptEntry>::new()
        );
        let toast = toast_line(
            app.ctrl_c_exit_armed,
            app.transcript_view.selection.as_ref(),
        )
        .expect("exit warning toast");
        assert_eq!(line_text(&toast), format!(" {CTRL_C_WARNING_TEXT} "));
        assert_eq!(toast.spans[1].style.fg, Some(CTRL_C_WARNING_FG));
        assert!(!line_text(&status_line(&app.snapshot())).contains(CTRL_C_WARNING_TEXT));

        let _ = app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.should_quit(), true);
    }

    #[test]
    fn tui_app_supports_cursor_multiline_word_delete_and_undo_redo() {
        let mut app = TuiApp::new(UiSnapshot::default());

        for ch in "hello world".chars() {
            let _ = app.handle_key(key(KeyCode::Char(ch)));
        }
        let _ = app.handle_key(modified_key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        let _ = app.handle_key(key(KeyCode::Char('X')));
        assert_eq!(app.snapshot().input, "Xhello world");

        let _ = app.handle_key(modified_key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        let _ = app.handle_key(modified_key(KeyCode::Enter, KeyModifiers::SHIFT));
        let _ = app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.snapshot().input, "Xhello world\nn");

        let _ = app.handle_key(modified_key(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(app.snapshot().input, "Xhello world\n");

        let _ = app.handle_key(modified_key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.snapshot().input, "Xhello world\nn");

        let _ = app.handle_key(modified_key(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(app.snapshot().input, "Xhello world\n");
    }

    #[test]
    fn tui_app_ctrl_horizontal_arrows_move_by_word() {
        let mut app = TuiApp::new(UiSnapshot {
            input: "alpha beta gamma".to_string(),
            input_cursor: "alpha beta gamma".len(),
            ..Default::default()
        });

        let _ = app.handle_key(modified_key(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(app.snapshot.input_cursor, "alpha beta ".len());

        let _ = app.handle_key(modified_key(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(app.snapshot.input_cursor, "alpha ".len());

        let _ = app.handle_key(modified_key(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.snapshot.input_cursor, "alpha beta ".len());
    }

    #[test]
    fn tui_app_shift_arrows_select_and_text_input_replaces_selection() {
        let mut app = TuiApp::new(UiSnapshot {
            input: "abc".to_string(),
            input_cursor: 1,
            ..Default::default()
        });

        let _ = app.handle_key(modified_key(KeyCode::Right, KeyModifiers::SHIFT));
        assert_eq!(app.input_editor.selection_range(&app.snapshot), Some(1..2));

        let _ = app.handle_key(key(KeyCode::Char('X')));
        assert_eq!(app.snapshot.input, "aXc");
        assert_eq!(app.snapshot.input_cursor, 2);
        assert_eq!(app.input_editor.selection_range(&app.snapshot), None);
    }

    #[test]
    fn tui_app_ctrl_shift_horizontal_arrows_select_words() {
        let mut app = TuiApp::new(UiSnapshot {
            input: "alpha beta gamma".to_string(),
            input_cursor: "alpha ".len(),
            ..Default::default()
        });

        let _ = app.handle_key(modified_key(
            KeyCode::Right,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(
            app.input_editor.selection_range(&app.snapshot),
            Some("alpha ".len().."alpha beta ".len())
        );

        let _ = app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.snapshot.input, "alpha gamma");
        assert_eq!(app.snapshot.input_cursor, "alpha ".len());
    }

    #[test]
    fn input_mouse_selection_updates_input_selection_without_transcript_selection() {
        let mut app = TuiApp::new(UiSnapshot {
            input: "hello world".to_string(),
            input_cursor: 0,
            transcript_entries: vec![text_entry("assistant: keep transcript unselected")],
            ..Default::default()
        });
        app.last_input_content_area = Rect {
            x: 10,
            y: 5,
            width: 20,
            height: 3,
        };

        let _ = app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 10, 5));
        let _ = app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 15, 5));
        let _ = app.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), 15, 5));

        assert_eq!(app.input_editor.selection_range(&app.snapshot), Some(0..5));
        assert!(app.transcript_view.selection().is_none());
    }

    #[test]
    fn input_selection_rendering_marks_selected_spans() {
        let lines = input_visual_lines_with_selection("hello", 10, Some(1..4));

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "h");
        assert_eq!(lines[0].spans[1].content, "ell");
        assert_eq!(lines[0].spans[1].style.bg, Some(INPUT_SELECTION_BG));
        assert_eq!(lines[0].spans[2].content, "o");
    }

    #[test]
    fn input_mouse_position_maps_soft_wrapped_rows_to_byte_indices() {
        assert_eq!(input_byte_index_for_visual_position("abcdef", 3, 1, 1), 4);
    }

    #[test]
    fn input_cursor_scrolls_when_multiline_input_exceeds_area() {
        let mut snapshot = UiSnapshot {
            input: "one\ntwo\nthree\nfour\nfive".to_string(),
            input_cursor: "one\ntwo\nthree\nfour\nfive".len(),
            ..Default::default()
        };
        clamp_input_cursor(&mut snapshot);
        let area = Rect {
            x: 10,
            y: 20,
            width: 40,
            height: 3,
        };

        let scroll = input_scroll_offset(&snapshot, area);
        let cursor = input_cursor_position(&snapshot, area, scroll).expect("cursor position");

        assert_eq!(scroll, 2);
        assert_eq!(cursor.x, 14);
        assert_eq!(cursor.y, 22);
    }

    #[test]
    fn input_cursor_and_height_follow_soft_wrapped_lines() {
        let mut snapshot = UiSnapshot {
            input: "abcdefghijk".to_string(),
            input_cursor: "abcdefghijk".len(),
            ..Default::default()
        };
        clamp_input_cursor(&mut snapshot);
        let area = Rect {
            x: 5,
            y: 10,
            width: 4,
            height: 2,
        };

        let scroll = input_scroll_offset(&snapshot, area);
        let cursor = input_cursor_position(&snapshot, area, scroll).expect("cursor position");

        assert_eq!(input_visual_line_count(&snapshot.input, area.width), 3);
        assert_eq!(scroll, 1);
        assert_eq!(cursor.x, 8);
        assert_eq!(cursor.y, 11);

        let frame_area = Rect {
            x: 0,
            y: 0,
            width: 5,
            height: 20,
        };
        assert_eq!(input_area_height(frame_area, &snapshot), 4);

        snapshot.input = "abcd".to_string();
        snapshot.input_cursor = "abcd".len();
        let boundary_cursor = input_cursor_position(&snapshot, area, 0).expect("cursor position");

        assert_eq!(boundary_cursor.x, 5);
        assert_eq!(boundary_cursor.y, 11);

        let visual_lines = input_visual_lines("hello world", 6)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(
            visual_lines,
            vec!["hello ".to_string(), "world".to_string()]
        );
    }

    #[test]
    fn input_area_starts_at_three_lines_and_caps_at_forty_percent() {
        let empty = UiSnapshot::default();
        let frame_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 100,
        };
        assert_eq!(input_area_height(frame_area, &empty), 3);

        let four_lines = UiSnapshot {
            input: "one\ntwo\nthree\nfour".to_string(),
            ..Default::default()
        };
        assert_eq!(input_area_height(frame_area, &four_lines), 4);

        let many_lines = UiSnapshot {
            input: (0..60)
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            ..Default::default()
        };
        assert_eq!(input_area_height(frame_area, &many_lines), 40);
    }

    #[test]
    fn input_left_margin_reserves_two_columns_before_text() {
        let snapshot = UiSnapshot {
            input: "abcd".to_string(),
            input_cursor: 4,
            ..Default::default()
        };
        let area = Rect {
            x: 10,
            y: 20,
            width: 40,
            height: 3,
        };
        let margin = input_margin_area(area);
        let content = input_content_area(area);
        let cursor = input_cursor_position(&snapshot, content, 0).expect("cursor position");

        assert_eq!(margin.x, 10);
        assert_eq!(margin.width, 2);
        assert_eq!(content.x, 12);
        assert_eq!(content.width, 38);
        assert_eq!(cursor.x, 16);
    }

    #[test]
    fn input_margin_marks_vertical_overflow() {
        let snapshot = UiSnapshot {
            input: (0..7)
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            ..Default::default()
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 3,
        };
        let lines = input_margin_lines(&snapshot, area, 2, 10);

        assert_eq!(lines[0].spans[0].content, "▲ ");
        assert_eq!(lines[1].spans[0].content, "│ ");
        assert_eq!(lines[2].spans[0].content, "▼ ");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Gray));
    }

    #[test]
    fn activity_summary_lines_show_working_queued_and_capped_agents() {
        let snapshot = UiSnapshot {
            response_streaming: true,
            queued_steering_prompt: Some("prefer tests".to_string()),
            agents: vec![
                harness_core::subagents::AgentSummary {
                    id: harness_core::subagents::AgentId(1),
                    path: "/root/a".to_string(),
                    status: AgentStatus::Running,
                    last_task_message: None,
                    last_activity_message: Some("reading".to_string()),
                },
                harness_core::subagents::AgentSummary {
                    id: harness_core::subagents::AgentId(2),
                    path: "/root/b".to_string(),
                    status: AgentStatus::Waiting,
                    last_task_message: None,
                    last_activity_message: None,
                },
            ],
            ..Default::default()
        };

        let lines = activity_summary_lines(&snapshot, Some(Duration::from_secs(19)));

        assert_eq!(lines.len(), 3);
        assert_eq!(line_text(&lines[0]), "• Working (19s • esc to interrupt)");
        assert_eq!(lines[0].spans[1].style.fg, Some(ACTIVITY_SECONDARY_FG));
        assert_eq!(
            line_text(&lines[1]),
            "queued prefer tests (esc to steer instantly)"
        );
        assert_eq!(line_text(&lines[2]), "agent /root/a and 1 more");
    }
    #[test]
    fn activity_summary_lines_show_persistent_working_after_stream_completion() {
        let snapshot = UiSnapshot {
            response_streaming: false,
            ..Default::default()
        };

        let lines = activity_summary_lines(&snapshot, Some(Duration::from_secs(7)));

        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "• Working (7s)");
        assert_eq!(lines[0].spans[1].style.fg, Some(ACTIVITY_SECONDARY_FG));
    }

    #[test]
    fn activity_summary_lines_show_appended_queued_messages_on_separate_rows() {
        let snapshot = UiSnapshot {
            queued_steering_prompt: Some("first\nsecond".to_string()),
            ..Default::default()
        };

        let lines = activity_summary_lines(&snapshot, None);

        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[0]), "queued first");
        assert_eq!(
            line_text(&lines[1]),
            "       second (esc to steer instantly)"
        );
    }

    #[test]
    fn activity_panel_lines_add_gutter_and_background() {
        let lines = activity_panel_lines(vec![Line::from(vec![Span::styled(
            "agent /root/a running",
            Style::default().fg(ACTIVITY_PRIMARY_FG),
        )])]);

        assert_eq!(line_text(&lines[0]), "│ agent /root/a running");
        assert_eq!(lines[0].spans[0].style.fg, Some(ACTIVITY_GUTTER_FG));
        assert_eq!(lines[0].spans[0].style.bg, Some(ACTIVITY_AREA_BG));
        assert_eq!(lines[0].spans[1].style.bg, Some(ACTIVITY_AREA_BG));
    }

    #[test]
    fn activity_summary_lines_include_failed_agent_message() {
        let snapshot = UiSnapshot {
            agents: vec![harness_core::subagents::AgentSummary {
                id: harness_core::subagents::AgentId(1),
                path: "/root/failed".to_string(),
                status: AgentStatus::Failed("responses actor error: closed".to_string()),
                last_task_message: None,
                last_activity_message: Some("agent failed".to_string()),
            }],
            ..Default::default()
        };

        let lines = activity_summary_lines(&snapshot, None);

        assert_eq!(
            line_text(&lines[0]),
            "agent /root/failed failed: responses actor error: closed agent failed"
        );
    }

    #[test]
    fn activity_summary_lines_are_empty_without_activity_or_agents() {
        let lines = activity_summary_lines(&UiSnapshot::default(), None);

        assert_eq!(lines, Vec::<Line<'static>>::new());
    }

    #[test]
    fn transcript_viewport_renders_tail_and_scrolls_up() {
        let entries = (0..10)
            .map(|index| text_entry(&format!("assistant: line-{index}")))
            .collect::<Vec<_>>();

        let tail = transcript_viewport(&entries, 3, 80, 0, None);
        assert_eq!(tail.total_lines, 19);
        assert_eq!(tail.top_line, 16);
        assert_eq!(tail.scroll_offset, 0);
        assert!(line_text(&tail.lines[0]).contains("line-8"));
        assert_eq!(line_text(&tail.lines[1]), "");
        assert!(line_text(&tail.lines[2]).contains("line-9"));

        let scrolled = transcript_viewport(&entries, 3, 80, 2, None);
        assert_eq!(scrolled.top_line, 14);
        assert_eq!(scrolled.scroll_offset, 2);
        assert!(line_text(&scrolled.lines[0]).contains("line-7"));
        assert_eq!(line_text(&scrolled.lines[1]), "");
        assert!(line_text(&scrolled.lines[2]).contains("line-8"));
    }

    #[test]
    fn transcript_viewport_wraps_rendered_lines_to_content_width() {
        let entries = vec![text_entry("assistant: abcdef")];

        let viewport = transcript_viewport(&entries, 4, 6, 0, None);

        assert_eq!(viewport.total_lines, 2);
        assert_eq!(line_text(&viewport.lines[0]), " • abc");
        assert_eq!(line_text(&viewport.lines[1]), "   def");
        assert_eq!(
            viewport.metadata,
            vec![
                TranscriptLineMetadata {
                    visual_line: 0,
                    content: Some("abc".to_string()),
                },
                TranscriptLineMetadata {
                    visual_line: 1,
                    content: Some("def".to_string()),
                },
            ]
        );

        let wide = transcript_viewport(&[text_entry("assistant: 好a")], 4, 5, 0, None);
        assert_eq!(wide.total_lines, 2);
        assert_eq!(line_text(&wide.lines[0]), " • 好");
        assert_eq!(line_text(&wide.lines[1]), "   a");
    }

    #[test]
    fn transcript_viewport_reserves_scrollbar_column_when_wrapping() {
        let entries = vec![text_entry("assistant: abcd"), text_entry("assistant: ef")];

        let viewport = transcript_viewport(&entries, 1, 7, usize::MAX, None);

        assert_eq!(viewport.total_lines, 4);
        assert_eq!(line_text(&viewport.lines[0]), " • abc");
    }

    #[test]
    fn transcript_scrollbar_uses_single_stable_position() {
        let top = transcript_scrollbar_lines(100, 10, 0, 5);
        let middle = transcript_scrollbar_lines(100, 10, 45, 5);
        let bottom = transcript_scrollbar_lines(100, 10, 90, 5);

        assert_eq!(top.iter().filter(|line| line_text(line) == "█").count(), 1);
        assert_eq!(
            middle.iter().filter(|line| line_text(line) == "█").count(),
            1
        );
        assert_eq!(
            bottom.iter().filter(|line| line_text(line) == "█").count(),
            1
        );
        assert_eq!(line_text(&top[0]), "█");
        assert_eq!(line_text(&middle[2]), "█");
        assert_eq!(line_text(&bottom[4]), "█");
        assert_eq!(line_text(&top[1]), "");
        assert_eq!(line_text(&middle[0]), "");
        assert_eq!(line_text(&bottom[3]), "");
    }

    #[test]
    fn transcript_scrollbar_is_hidden_without_overflow() {
        assert!(transcript_scrollbar_lines(3, 3, 0, 5).is_empty());
    }

    #[test]
    fn transcript_scrollbar_visibility_expires_after_duration() {
        let mut app = TuiApp::new(UiSnapshot::default());
        let now = Instant::now();
        app.reveal_transcript_scrollbar_at(now);

        assert!(app.transcript_scrollbar_visible_at(
            now + TRANSCRIPT_SCROLLBAR_VISIBLE_DURATION - Duration::from_millis(1)
        ));
        assert!(!app.transcript_scrollbar_visible_at(now + TRANSCRIPT_SCROLLBAR_VISIBLE_DURATION));

        app.hide_transcript_scrollbar();
        assert!(!app.transcript_scrollbar_visible_at(now));
    }

    #[test]
    fn tui_app_page_keys_scroll_transcript_without_editing_input() {
        let snapshot = UiSnapshot {
            transcript_entries: (0..30)
                .map(|index| text_entry(&format!("assistant: line-{index}")))
                .collect(),
            input: "keep".to_string(),
            input_cursor: 4,
            ..Default::default()
        };
        let mut app = TuiApp::new(snapshot);
        app.transcript_view.last_view_height = 5;

        let _ = app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.transcript_view.scroll_offset, 4);
        assert_eq!(app.snapshot.input, "keep");

        let _ = app.handle_key(modified_key(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(app.transcript_view.scroll_offset, 54);

        let _ = app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.transcript_view.scroll_offset, 50);

        let _ = app.handle_key(modified_key(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(app.transcript_view.scroll_offset, 0);
    }

    #[test]
    fn tui_app_mouse_wheel_scrolls_transcript() {
        let snapshot = UiSnapshot {
            transcript_entries: (0..30)
                .map(|index| text_entry(&format!("assistant: line-{index}")))
                .collect(),
            input: "keep".to_string(),
            input_cursor: 4,
            ..Default::default()
        };
        let mut app = TuiApp::new(snapshot);
        app.transcript_view.last_view_height = 5;

        let _ = app.handle_mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(
            app.transcript_view.scroll_offset,
            TRANSCRIPT_MOUSE_SCROLL_LINES
        );
        assert_eq!(app.snapshot.input, "keep");

        let _ = app.handle_mouse(mouse(MouseEventKind::ScrollDown));
        assert_eq!(app.transcript_view.scroll_offset, 0);
    }

    #[test]
    fn transcript_selection_copies_content_without_decorations() {
        let entries = vec![
            text_entry("assistant: hello"),
            freeform_tool_call("custom_tool", "arg: true"),
            freeform_tool_output("ok"),
        ];
        let selection = TranscriptSelection {
            anchor: TranscriptSelectionPoint {
                visual_line: 0,
                content_col: 0,
            },
            cursor: TranscriptSelectionPoint {
                visual_line: 3,
                content_col: 9,
            },
        };

        let text = selected_transcript_text(&entries, &selection).unwrap();

        assert_eq!(text, "hello\n\ncustom_tool\narg: true");
        assert!(!text.contains("call-1"));
        assert!(!text.contains(TRANSCRIPT_ASSISTANT_MARKER));
        assert!(!text.contains(TRANSCRIPT_TOOL_MARKER));
    }

    #[test]
    fn transcript_selection_highlight_applies_only_to_content_spans() {
        let line = transcript_entry_lines(text_entry("assistant: hello"))
            .into_iter()
            .next()
            .unwrap();
        let selection = TranscriptSelection {
            anchor: TranscriptSelectionPoint {
                visual_line: 0,
                content_col: 1,
            },
            cursor: TranscriptSelectionPoint {
                visual_line: 0,
                content_col: 4,
            },
        };
        let (start, end) = selection_range_for_line(&selection, 0, "hello".chars().count())
            .expect("selection range");

        let selected = apply_selection_to_transcript_line(line, start, end);

        assert!(
            !selected.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            selected
                .spans
                .iter()
                .skip(3)
                .any(|span| span.style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    #[test]
    fn transcript_mouse_selection_updates_content_range() {
        let mut app = TuiApp::new(UiSnapshot {
            transcript_entries: vec![text_entry("assistant: hello")],
            ..Default::default()
        });
        app.transcript_view.last_area = Rect {
            x: 5,
            y: 2,
            width: 40,
            height: 5,
        };
        app.transcript_view.last_view_height = 5;

        let _ = app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 8, 2));
        let _ = app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 13, 2));
        let _ = app.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), 13, 2));

        assert_eq!(app.selected_transcript_text(), Some("hello".to_string()));
    }

    #[test]
    fn transcript_selection_drag_near_top_scrolls_up() {
        let mut app = TuiApp::new(UiSnapshot {
            transcript_entries: (0..30)
                .map(|index| text_entry(&format!("assistant: line-{index}")))
                .collect(),
            ..Default::default()
        });
        app.transcript_view.last_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 5,
        };
        app.transcript_view.last_view_height = 5;
        app.transcript_view.scroll_offset = 3;

        let _ = app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 2));
        let _ = app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 0));

        assert_eq!(app.transcript_view.scroll_offset, 4);
    }

    #[test]
    fn ctrl_shift_c_copies_transcript_selection() {
        let mut app = TuiApp::new(UiSnapshot {
            transcript_entries: vec![text_entry("assistant: hello")],
            ..Default::default()
        });
        app.transcript_view.selection = Some(TranscriptSelection {
            anchor: TranscriptSelectionPoint {
                visual_line: 0,
                content_col: 0,
            },
            cursor: TranscriptSelectionPoint {
                visual_line: 0,
                content_col: 5,
            },
        });

        let action = app.handle_key(modified_key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));

        assert_eq!(action, Some(TuiAction::CopySelection("hello".to_string())));
    }

    #[test]
    fn y_yanks_transcript_selection() {
        let mut app = TuiApp::new(UiSnapshot {
            transcript_entries: vec![text_entry("assistant: hello")],
            ..Default::default()
        });
        app.transcript_view.selection = Some(TranscriptSelection {
            anchor: TranscriptSelectionPoint {
                visual_line: 0,
                content_col: 0,
            },
            cursor: TranscriptSelectionPoint {
                visual_line: 0,
                content_col: 5,
            },
        });

        let action = app.handle_key(key(KeyCode::Char('y')));

        assert_eq!(action, Some(TuiAction::CopySelection("hello".to_string())));
        assert_eq!(app.snapshot().input, "");
    }

    #[test]
    fn y_inserts_text_without_transcript_selection() {
        let mut app = TuiApp::new(UiSnapshot::default());

        let action = app.handle_key(key(KeyCode::Char('y')));

        assert_eq!(action, None);
        assert_eq!(app.snapshot().input, "y");
    }

    #[test]
    fn osc52_copy_encodes_clipboard_payload() {
        let mut output = Vec::new();

        copy_to_clipboard_osc52(&mut output, "hello").unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\u{1b}]52;c;aGVsbG8=\u{7}"
        );
    }

    #[test]
    fn tui_app_requests_transcript_page_at_retained_top() {
        let snapshot = UiSnapshot {
            transcript_entries: (0..30)
                .map(|index| text_entry(&format!("assistant: line-{index}")))
                .collect(),
            ..Default::default()
        };
        let mut app = TuiApp::new(snapshot);
        app.transcript_view.last_view_height = 5;

        let action = app.handle_key(modified_key(KeyCode::Home, KeyModifiers::CONTROL));

        assert_eq!(
            action,
            Some(TuiAction::LoadTranscriptPage {
                before_seq: None,
                max_lines: TRANSCRIPT_PAGE_LINE_LIMIT,
            })
        );
        assert_eq!(app.transcript_view.page_loading, true);
        assert_eq!(
            app.handle_key(modified_key(KeyCode::Home, KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn tui_app_prepends_loaded_transcript_page() {
        let snapshot = UiSnapshot {
            transcript_entries: (0..10)
                .map(|index| text_entry(&format!("assistant: current-{index}")))
                .collect(),
            ..Default::default()
        };
        let mut app = TuiApp::new(snapshot);
        app.transcript_view.last_view_height = 5;
        let _ = app.handle_key(modified_key(KeyCode::Home, KeyModifiers::CONTROL));

        app.apply_runtime_event(RuntimeEvent::TranscriptPage(TranscriptPage {
            lines: vec![
                transcript_page_line(2, session_user_entry("user> older")),
                transcript_page_line(3, session_assistant_entry("assistant: older answer")),
            ],
            next_before_seq: Some(2),
            reached_start: false,
        }));

        let snapshot = app.snapshot();
        assert_eq!(
            snapshot.transcript_entries[0],
            session_user_entry("user> older")
        );
        assert_eq!(
            snapshot.transcript_entries[1],
            session_assistant_entry("assistant: older answer")
        );
        assert_eq!(app.transcript_view.page_before_seq, Some(2));
        assert_eq!(app.transcript_view.page_loading, false);
        assert_eq!(app.transcript_view.scroll_offset, 18);
    }

    #[test]
    fn tui_app_deduplicates_loaded_page_overlap() {
        let mut app = TuiApp::new(UiSnapshot {
            transcript_entries: vec![session_user_entry("user> current")],
            ..Default::default()
        });
        app.apply_runtime_event(RuntimeEvent::TranscriptPage(TranscriptPage {
            lines: vec![
                transcript_page_line(1, session_user_entry("user> older")),
                transcript_page_line(2, session_user_entry("user> current")),
            ],
            next_before_seq: Some(1),
            reached_start: true,
        }));

        assert_eq!(
            app.snapshot().transcript_entries,
            vec![
                session_user_entry("user> older"),
                session_user_entry("user> current"),
            ]
        );
    }

    #[test]
    fn tui_app_autoscrolls_only_when_following_tail() {
        let snapshot = UiSnapshot {
            transcript_entries: (0..30)
                .map(|index| text_entry(&format!("assistant: line-{index}")))
                .collect(),
            ..Default::default()
        };
        let mut app = TuiApp::new(snapshot);
        app.transcript_view.last_view_height = 5;

        app.apply_runtime_event(RuntimeEvent::TranscriptLine(
            "assistant: new-tail".to_string(),
        ));
        assert_eq!(app.transcript_view.scroll_offset, 0);

        let _ = app.handle_mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(
            app.transcript_view.scroll_offset,
            TRANSCRIPT_MOUSE_SCROLL_LINES
        );

        app.apply_runtime_event(RuntimeEvent::TranscriptLine(
            "assistant: preserve-view".to_string(),
        ));
        assert_eq!(
            app.transcript_view.scroll_offset,
            TRANSCRIPT_MOUSE_SCROLL_LINES + 2
        );
    }

    #[test]
    fn tui_app_bounds_in_memory_transcript_tail() {
        let snapshot = UiSnapshot {
            transcript_entries: (0..MAX_TRANSCRIPT_ENTRIES + 3)
                .map(|index| text_entry(&format!("assistant: line-{index}")))
                .collect(),
            ..Default::default()
        };

        let app = TuiApp::new(snapshot);

        let snapshot = app.snapshot();
        assert_eq!(snapshot.transcript_entries.len(), MAX_TRANSCRIPT_ENTRIES);
        assert_eq!(
            snapshot.transcript_entries[0],
            text_entry("assistant: line-3")
        );
        assert!(transcript_total_bytes(&snapshot.transcript_entries) <= MAX_TRANSCRIPT_BYTES);
    }

    #[test]
    fn tui_app_bounds_in_memory_transcript_bytes() {
        let entry_body = "x".repeat(16 * 1024);
        let entry_count = MAX_TRANSCRIPT_BYTES / entry_body.len() + 20;
        let snapshot = UiSnapshot {
            transcript_entries: (0..entry_count)
                .map(|index| text_entry(&format!("assistant: {index} {entry_body}")))
                .collect(),
            ..Default::default()
        };

        let app = TuiApp::new(snapshot);

        let snapshot = app.snapshot();
        assert!(transcript_total_bytes(&snapshot.transcript_entries) <= MAX_TRANSCRIPT_BYTES);
        assert!(snapshot.transcript_entries.len() < entry_count);
    }

    #[test]
    fn transcript_entry_trimming_preserves_role_prefix_and_tail() {
        let mut entry = format!(
            "assistant: {}tail",
            "x".repeat(MAX_TRANSCRIPT_ENTRY_BYTES + 128)
        );

        trim_transcript_entry(&mut entry);

        assert!(entry.len() <= MAX_TRANSCRIPT_ENTRY_BYTES);
        assert!(entry.capacity() <= MAX_TRANSCRIPT_ENTRY_BYTES);
        assert!(entry.starts_with("assistant: "));
        assert!(entry.contains(TRANSCRIPT_TRUNCATED_MARKER));
        assert!(entry.ends_with("tail"));
    }

    #[test]
    fn status_line_is_bottom_row_below_input() {
        let [_, _, input, status] = ui_areas(
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            3,
            0,
        );

        assert_eq!(input.y, 20);
        assert_eq!(input.height, 3);
        assert_eq!(status.y, 23);
        assert_eq!(status.height, 1);
    }

    #[test]
    fn activity_area_only_reserves_rows_for_visible_activity() {
        let [
            main_without_activity,
            activity_without_activity,
            input_without_activity,
            _,
        ] = ui_areas(
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            3,
            0,
        );
        let [
            main_with_activity,
            activity_with_activity,
            input_with_activity,
            _,
        ] = ui_areas(
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            3,
            2,
        );

        assert_eq!(activity_without_activity.height, 0);
        assert_eq!(main_without_activity.height, 20);
        assert_eq!(input_without_activity.y, 20);
        assert_eq!(activity_with_activity.height, 2);
        assert_eq!(main_with_activity.height, 18);
        assert_eq!(input_with_activity.y, 20);
    }

    #[test]
    fn status_line_includes_session_id() {
        let snapshot = UiSnapshot {
            session_id: "session-test".to_string(),
            response_streaming: true,
            last_ttft_ms: Some(142),
            ..Default::default()
        };
        let line = status_line(&snapshot);

        assert_eq!(
            line_text(&line),
            "session: session-test  │  provider: —  │  model: gpt-5.5 · xhigh · fast  │  ctx: —"
        );
        assert_eq!(line.spans[0].style.fg, Some(STATUS_LABEL_FG));
        assert_eq!(line.spans[1].style.fg, Some(STATUS_VALUE_FG));
    }

    #[test]
    fn status_line_omits_role_idle_and_ttft() {
        let line = status_line(&UiSnapshot::default());
        let text = line_text(&line);

        assert!(text.contains("ctx: —"));
        assert!(!text.contains("role:"));
        assert!(!text.contains("idle"));
        assert!(!text.contains("ttft:"));
    }
    #[test]
    fn status_line_shows_context_usage() {
        let snapshot = UiSnapshot::default();
        let line = status_line_with_context(
            &snapshot,
            Some(ContextWindowUsage {
                estimated_input_tokens: 12_345,
                max_input_tokens: 258_400,
                compact_at_tokens: 245_480,
                target_tokens_after_compaction: 122_740,
            }),
        );

        assert!(line_text(&line).contains("ctx: 12.3k/258.4k"));
        assert_eq!(line.spans[14].style.fg, Some(STATUS_VALUE_FG));
    }

    #[test]
    fn status_line_warns_when_context_usage_reaches_compaction_threshold() {
        let snapshot = UiSnapshot::default();
        let line = status_line_with_context(
            &snapshot,
            Some(ContextWindowUsage {
                estimated_input_tokens: 245_480,
                max_input_tokens: 258_400,
                compact_at_tokens: 245_480,
                target_tokens_after_compaction: 122_740,
            }),
        );

        assert_eq!(line.spans[14].style.fg, Some(STATUS_CONTEXT_WARNING_FG));
    }

    #[test]
    fn toast_line_shows_yank_and_exit_hints() {
        let selection = TranscriptSelection {
            anchor: TranscriptSelectionPoint {
                visual_line: 0,
                content_col: 0,
            },
            cursor: TranscriptSelectionPoint {
                visual_line: 0,
                content_col: 1,
            },
        };

        let toast = toast_line(true, Some(&selection)).expect("toast");

        assert_eq!(
            line_text(&toast),
            format!(" {YANK_SELECTION_TEXT}  ·  {CTRL_C_WARNING_TEXT} ")
        );
    }

    #[test]
    fn toast_area_places_toast_at_top_right() {
        let toast = toast_line(true, None).expect("toast");

        let area = toast_area(
            Rect {
                x: 2,
                y: 3,
                width: 80,
                height: 10,
            },
            &toast,
        )
        .expect("toast area");

        assert_eq!(area.y, 3);
        assert_eq!(area.height, 1);
        assert_eq!(area.x + area.width, 82);
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn tui_app_moves_cursor_up_and_down_in_multiline_input() {
        let mut app = TuiApp::new(UiSnapshot::default());
        app.snapshot.input = "abcd\nef\nghijk".to_string();
        app.snapshot.input_cursor = "abcd\nef\nghij".len();

        let _ = app.handle_key(key(KeyCode::Up));
        assert_eq!(app.snapshot.input_cursor, "abcd\nef".len());

        let _ = app.handle_key(key(KeyCode::Up));
        assert_eq!(app.snapshot.input_cursor, "abcd".len());

        let _ = app.handle_key(key(KeyCode::Down));
        assert_eq!(app.snapshot.input_cursor, "abcd\nef".len());

        let _ = app.handle_key(key(KeyCode::Down));
        assert_eq!(app.snapshot.input_cursor, "abcd\nef\nghij".len());
    }

    #[test]
    fn tui_app_does_not_insert_control_modified_chars() {
        let mut app = TuiApp::new(UiSnapshot::default());

        let _ = app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

        assert_eq!(app.snapshot().input, "");
    }

    #[test]
    fn tui_app_applies_runtime_transcript_events() {
        let mut app = TuiApp::new(UiSnapshot::default());

        app.apply_runtime_event(RuntimeEvent::TranscriptLine("assistant: hi".to_string()));

        assert_eq!(
            app.snapshot().transcript_entries,
            vec![text_entry("assistant: hi")]
        );
    }

    #[test]
    fn tui_app_rematerializes_snapshot_transcript_from_view_store() {
        let mut app = TuiApp::new(UiSnapshot {
            transcript_entries: vec![text_entry("user> restored")],
            input: "draft".to_string(),
            input_cursor: 5,
            ..Default::default()
        });

        assert!(app.snapshot.transcript_entries.is_empty());
        assert_eq!(
            app.transcript_view.entries(),
            &[text_entry("user> restored")]
        );

        app.apply_runtime_event(RuntimeEvent::ResponseStreamStarted);
        app.apply_runtime_event(RuntimeEvent::AssistantTextDelta("continued".to_string()));

        assert_eq!(
            app.snapshot().transcript_entries,
            vec![
                text_entry("user> restored"),
                text_entry("assistant: continued")
            ]
        );

        let snapshot = app.into_snapshot();
        assert_eq!(
            snapshot.transcript_entries,
            vec![
                text_entry("user> restored"),
                text_entry("assistant: continued")
            ]
        );
        assert_eq!(snapshot.input, "draft");
        assert_eq!(snapshot.input_cursor, 5);
    }

    #[test]
    fn tui_app_applies_model_settings_events() {
        let mut app = TuiApp::new(UiSnapshot::default());

        app.apply_runtime_event(RuntimeEvent::ModelSettingsChanged(
            harness_core::responses::ModelSettings::new(
                "5.5",
                Some("xhigh".to_string()),
                Some("priority".to_string()),
            ),
        ));

        assert_eq!(app.snapshot().thread_title, "new_harness · gpt-5.5");
        assert_eq!(app.snapshot().model_settings.model, "gpt-5.5");
    }

    #[test]
    fn tui_app_streams_assistant_delta_and_ttft() {
        let mut app = TuiApp::new(UiSnapshot::default());

        app.apply_runtime_event(RuntimeEvent::ResponseStreamStarted);
        assert_eq!(app.snapshot().response_streaming, true);
        assert_eq!(ttft_label(&app.snapshot()), "pending");

        app.apply_runtime_event(RuntimeEvent::AssistantFirstToken { ttft_ms: 42 });
        app.apply_runtime_event(RuntimeEvent::AssistantTextDelta("hi".to_string()));
        app.apply_runtime_event(RuntimeEvent::AssistantTextDelta(" there".to_string()));

        assert_eq!(app.snapshot().last_ttft_ms, Some(42));
        assert_eq!(
            app.snapshot().transcript_entries,
            vec![text_entry("assistant: hi there")]
        );

        app.apply_runtime_event(RuntimeEvent::ResponseStreamCompleted);
        assert_eq!(app.snapshot().response_streaming, false);
        assert_eq!(ttft_label(&app.snapshot()), "42ms");
    }

    #[test]
    fn tui_app_keeps_working_active_after_response_stream_completes() {
        let mut app = TuiApp::new(UiSnapshot::default());

        app.apply_runtime_event(RuntimeEvent::AgenticLoopStarted);
        app.apply_runtime_event(RuntimeEvent::ResponseStreamStarted);
        app.apply_runtime_event(RuntimeEvent::ResponseStreamCompleted);

        assert_eq!(app.snapshot().response_streaming, false);
        assert!(app.working_elapsed().is_some());

        app.apply_runtime_event(RuntimeEvent::AgenticLoopCompleted);

        assert!(app.working_elapsed().is_none());
    }

    #[test]
    fn assistant_delta_buffer_coalesces_adjacent_chunks_until_flush() {
        let mut app = TuiApp::new(UiSnapshot::default());
        let mut pending = None;

        apply_runtime_event_coalesced(&mut app, RuntimeEvent::ResponseStreamStarted, &mut pending);
        apply_runtime_event_coalesced(
            &mut app,
            RuntimeEvent::AssistantTextDelta("hel".to_string()),
            &mut pending,
        );
        apply_runtime_event_coalesced(
            &mut app,
            RuntimeEvent::AssistantTextDelta("lo".to_string()),
            &mut pending,
        );

        assert_eq!(
            app.snapshot().transcript_entries,
            Vec::<UiTranscriptEntry>::new()
        );
        flush_assistant_delta(&mut app, &mut pending);

        assert_eq!(
            app.snapshot().transcript_entries,
            vec![text_entry("assistant: hello")]
        );
        assert_eq!(app.transcript_view.store.revision(), 2);
    }

    #[test]
    fn transcript_view_retains_rendered_entries_until_entry_revision_changes() {
        let mut app = TuiApp::new(UiSnapshot {
            transcript_entries: vec![text_entry("assistant: first")],
            ..Default::default()
        });
        let entry_id = app.transcript_view.store.metadata()[0].id();

        {
            let viewport = app.transcript_view.viewport_ref(5, 80, true);
            assert_eq!(line_text(&viewport.lines[0]), " • first");
        }
        assert_eq!(
            app.transcript_view
                .render_cache
                .cached_line_index_revision(77),
            Some(app.transcript_view.store.revision())
        );
        assert!(
            app.transcript_view
                .render_cache
                .entries
                .contains_key(&entry_id)
        );
        assert_eq!(
            app.transcript_view.render_cache.cached_line_count_entries(),
            1
        );
        assert_eq!(
            app.transcript_view
                .render_cache
                .cached_line_count_revision(entry_id),
            Some(0)
        );
        let original_render_key = app
            .transcript_view
            .render_cache
            .entries
            .get(&entry_id)
            .expect("cached render entry")
            .render_key;
        assert_eq!(
            app.transcript_view.render_cache.cached_line_index_count(),
            1
        );

        {
            let narrow_viewport = app.transcript_view.viewport_ref(5, 6, true);
            assert_eq!(line_text(&narrow_viewport.lines[0]), " • fir");
            assert_eq!(line_text(&narrow_viewport.lines[1]), "   st");
        }
        assert_eq!(
            app.transcript_view
                .render_cache
                .cached_line_index_revision(3),
            Some(app.transcript_view.store.revision())
        );
        assert_ne!(
            app.transcript_view
                .render_cache
                .entries
                .get(&entry_id)
                .expect("cached render entry")
                .render_key,
            original_render_key
        );

        let mut overflowing_app = TuiApp::new(UiSnapshot {
            transcript_entries: (0..10)
                .map(|index| text_entry(&format!("assistant: overflow {index}")))
                .collect(),
            ..Default::default()
        });
        let _ = overflowing_app.transcript_view.viewport_ref(3, 80, true);
        assert_eq!(
            overflowing_app
                .transcript_view
                .render_cache
                .cached_line_index_revision(77),
            Some(overflowing_app.transcript_view.store.revision())
        );
        assert_eq!(
            overflowing_app
                .transcript_view
                .render_cache
                .cached_line_index_revision(76),
            Some(overflowing_app.transcript_view.store.revision())
        );
        assert_eq!(
            overflowing_app
                .transcript_view
                .render_cache
                .cached_line_index_count(),
            2
        );
        let original_render_key = app
            .transcript_view
            .render_cache
            .entries
            .get(&entry_id)
            .expect("cached render entry")
            .render_key;

        app.apply_runtime_event(RuntimeEvent::ResponseStreamStarted);
        app.apply_runtime_event(RuntimeEvent::AssistantTextDelta("second".to_string()));
        let _ = app.transcript_view.viewport_ref(5, 80, true);

        assert_eq!(
            app.transcript_view
                .render_cache
                .cached_line_index_revision(77),
            Some(app.transcript_view.store.revision())
        );
        assert_eq!(
            app.transcript_view.render_cache.cached_line_count_entries(),
            3
        );
        assert_eq!(
            app.transcript_view
                .render_cache
                .cached_line_count_revision(entry_id),
            Some(0)
        );
        assert_ne!(
            app.transcript_view
                .render_cache
                .entries
                .get(&entry_id)
                .expect("cached render entry")
                .render_key,
            original_render_key
        );
    }

    #[test]
    fn transcript_view_edit_primitives_rematerialize_and_invalidate_cached_view() {
        let mut app = TuiApp::new(UiSnapshot {
            transcript_entries: vec![
                text_entry("user> first"),
                text_entry("assistant: second"),
                text_entry("assistant: third"),
            ],
            ..Default::default()
        });
        let first = app.transcript_entry_id_at(0).expect("first entry id");
        let second = app.transcript_entry_id_at(1).expect("second entry id");

        let _ = app.transcript_view.viewport_ref(8, 80, true);
        let original_line_index_revision = app
            .transcript_view
            .render_cache
            .cached_line_index_revision(77)
            .expect("cached transcript line index");
        app.replace_transcript_entry(second, "assistant: edited".to_string())
            .expect("replace second entry");
        assert_ne!(
            app.transcript_view.store.revision(),
            original_line_index_revision
        );

        let inserted = app
            .insert_transcript_entry_after(first, "developer> inserted".to_string())
            .expect("insert after first");
        assert_eq!(app.transcript_view.store.index_of(inserted), Some(1));

        let removed = app
            .truncate_transcript_after_entry(inserted)
            .expect("truncate after inserted");
        assert!(removed.contains(&second));
        assert_eq!(
            app.snapshot().transcript_entries,
            vec![text_entry("user> first"), text_entry("developer> inserted")]
        );
    }

    #[test]
    fn transcript_entry_renders_apply_patch_as_codex_style_edit_block() {
        let lines = transcript_entry_lines(freeform_tool_call(
            "apply_patch",
            "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch",
        ));

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].spans[1].content, TRANSCRIPT_TOOL_MARKER);
        assert_eq!(lines[0].spans[1].style.fg, Some(TRANSCRIPT_TOOL_FG));
        assert_eq!(lines[0].spans[2].content, " ");
        assert_eq!(line_text(&lines[0]), " ⚙ • Edited src/lib.rs (+1 -1)");
        assert_eq!(line_text(&lines[1]), "     @@");
        assert_eq!(line_text(&lines[2]), "     -old");
        assert_eq!(line_text(&lines[3]), "     +new");
        assert_eq!(lines[2].spans[3].style.bg, Some(DIFF_REMOVED_BG));
        assert!(lines[2].spans[3].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(lines[3].spans[3].style.bg, Some(DIFF_ADDED_BG));
    }

    #[test]
    fn transcript_entry_renders_edit_file_call_as_edit_block() {
        let lines = transcript_entry_lines(freeform_tool_call(
            "edit_file",
            "*** Edit src/lib.rs\n*** Replace 2ab 3cd\nnew_two\nnew_three\n",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ • Edited src/lib.rs (+2 -2)");
        assert_eq!(line_text(&lines[1]), "     @@ lines 2-3");
        assert_eq!(line_text(&lines[2]), "     +new_two");
        assert_eq!(line_text(&lines[3]), "     +new_three");
        assert_eq!(lines[2].spans[3].style.bg, Some(DIFF_ADDED_BG));
    }

    #[test]
    fn transcript_entry_renders_edit_file_anchor_with_numeric_hash_suffix() {
        let lines = transcript_entry_lines(freeform_tool_call(
            "edit_file",
            "*** Edit crates/harness-tui/src/lib.rs\n*** Delete 268888 268888\n",
        ));

        assert_eq!(
            line_text(&lines[0]),
            " ⚙ • Edited crates/harness-tui/src/lib.rs (+0 -1)"
        );
        assert_eq!(line_text(&lines[1]), "     @@ lines 2688-2688");
    }

    #[test]
    fn transcript_entry_renders_inspect_read_job_as_compact_model_read_line() {
        let lines = transcript_entry_lines(freeform_tool_call(
            "inspect",
            "read crates/foo/src/lib.rs\n120+80",
        ));

        assert_eq!(
            line_text(&lines[0]),
            " ⚙ Read crates/foo/src/lib.rs:120-199"
        );
        assert_eq!(lines[0].spans[3].content, "Read");
        assert_eq!(lines[0].spans[3].style.fg, Some(Color::White));
        assert!(
            lines[0].spans[3]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[4].content, " ");
        assert_eq!(lines[0].spans[5].content, "crates/foo/src/lib.rs");
        assert_eq!(lines[0].spans[5].style.fg, Some(TRANSCRIPT_TOOL_FG));
        assert_eq!(lines[0].spans[6].content, ":120-199");
        assert_eq!(lines[0].spans[6].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn transcript_renders_multiple_inspect_read_jobs_without_blank_separators() {
        let entries = vec![freeform_tool_call(
            "inspect",
            "read src/ast.rs\n174+80\nread src/build.rs\n940+100\nread src/effectcheck.rs\n280+80",
        )];

        let viewport = transcript_viewport(&entries, 8, 80, 0, None);
        let rendered = viewport.lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                " ⚙ Read src/ast.rs:174-253",
                "   Read src/build.rs:940-1039",
                "   Read src/effectcheck.rs:280-359",
            ]
        );
    }

    #[test]
    fn transcript_renders_inspect_read_output_content_as_stylized_result() {
        let entries = vec![
            freeform_tool_call("inspect", "read crates/foo/src/lib.rs\n1+1"),
            freeform_tool_output_with_structured_display(
                "1a3fn secret() {}\n",
                ToolOutputDisplayRecord::InspectRead(vec![InspectReadDisplayRecord {
                    path: "crates/foo/src/lib.rs".to_string(),
                    start_line: 1,
                    lines: vec!["fn secret() {}".to_string()],
                    next: None,
                }]),
            ),
        ];

        let viewport = transcript_viewport(&entries, 8, 80, 0, None);
        let rendered = viewport.lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(rendered[1], " ⚙ Read crates/foo/src/lib.rs:1-1");
        assert_eq!(rendered[2], "   1 │ fn secret() {}");
    }

    #[test]
    fn transcript_tool_call_hides_call_id() {
        let lines = transcript_entry_lines(freeform_tool_call("custom_tool", "arg: true"));

        assert_eq!(line_text(&lines[0]), " ⚙ custom_tool");
        assert_eq!(line_text(&lines[1]), "   arg: true");
        assert!(!line_text(&lines[0]).contains("call-1"));
    }

    #[test]
    fn transcript_unknown_tool_uses_generic_tool_call_display() {
        let lines = transcript_entry_lines(freeform_tool_call(
            "custom_tool",
            "path: crates/harness-tui/src/lib.rs",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ custom_tool");
        assert_eq!(
            line_text(&lines[1]),
            "   path: crates/harness-tui/src/lib.rs"
        );
    }

    #[test]
    fn transcript_terminal_open_command_is_compact() {
        let lines = transcript_entry_lines(freeform_tool_call(
            "terminal_open",
            "workdir: /tmp/project\ncommand:\ncargo test -p harness-core",
        ));

        assert_eq!(lines[0].spans[3].content, "$");
        assert_eq!(lines[0].spans[3].style.fg, Some(Color::White));
        assert!(
            lines[0].spans[3]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[4].content, " ");
        assert_eq!(lines[0].spans[5].content, "cargo test -p harness-core");
        assert_eq!(lines[0].spans[5].style.fg, Some(TRANSCRIPT_TOOL_FG));
        assert_eq!(line_text(&lines[0]), " ⚙ $ cargo test -p harness-core");
    }

    #[test]
    fn transcript_terminal_write_renders_stdin() {
        let stdin = transcript_entry_lines(freeform_tool_call(
            "terminal_write",
            "terminal: 3\ninput: yes",
        ));
        assert_eq!(line_text(&stdin[0]), " ⚙ stdin yes");

        let read = transcript_entry_lines(freeform_tool_call("terminal_read", "terminal: 3"));
        assert!(read.is_empty());
    }

    #[test]
    fn transcript_inspect_commands_are_compact() {
        let lines = transcript_entry_lines(freeform_tool_call(
            "inspect",
            "pwd\nsearch -n \"needle|haystack\" crates/harness-core/src\nwhich crg\nps -p 1 -o pid,comm=",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ $ pwd");
        assert_eq!(
            line_text(&lines[1]),
            "   $ search -n \"needle|haystack\" crates/harness-core/src"
        );
        assert_eq!(line_text(&lines[2]), "   $ which crg");
        assert_eq!(line_text(&lines[3]), "   $ ps -p 1 -o pid,comm=");
    }

    #[test]
    fn transcript_terminal_open_shows_full_compound_command() {
        let lines = transcript_entry_lines(freeform_tool_call(
            "terminal_open",
            "command:\nsed -n '1,260p' crates/harness-tui/src/lib.rs && sed -n '1,220p' crates/harness-cli/src/main.rs && rg -n \"harness_tui|TuiApp\" crates",
        ));

        assert_eq!(
            line_text(&lines[0]),
            " ⚙ $ sed -n '1,260p' crates/harness-tui/src/lib.rs && sed -n '1,220p' crates/harness-cli/src/main.rs && rg -n \"harness_tui|TuiApp\" crates"
        );
    }

    #[test]
    fn transcript_terminal_open_shows_full_search_pipeline() {
        let lines = transcript_entry_lines(freeform_tool_call(
            "terminal_open",
            "workdir: /var/home/me/misc/new_harness\ncommand:\npwd; rg --files | rg -i 'ui|draft|frontend|app|component|view|widget|screen|page|panel' | head -200; rg -n \"draft|ui|UI\" crates",
        ));

        assert_eq!(
            line_text(&lines[0]),
            " ⚙ $ pwd; rg --files | rg -i 'ui|draft|frontend|app|component|view|widget|screen|page|panel' | head -200; rg -n \"draft|ui|UI\" crates"
        );
    }

    #[test]
    fn transcript_tool_output_hides_call_id_and_header() {
        let lines = transcript_entry_lines(freeform_tool_output("ok\nmore"));

        assert_eq!(line_text(&lines[0]), " ⚙ ok");
        assert_eq!(line_text(&lines[1]), "   more");
        assert!(!line_text(&lines[0]).contains("call-1"));
        assert!(!line_text(&lines[0]).contains("tool output"));
    }

    #[test]
    fn transcript_tool_output_hides_process_envelope() {
        let lines = transcript_entry_lines(freeform_tool_output(
            "Chunk ID: chunk-1\nWall time: 0.123 seconds\nProcess exited with code 0\nOutput:\nok\nmore",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ ok");
        assert_eq!(line_text(&lines[1]), "   more");
        assert!(
            !lines
                .iter()
                .any(|line| line_text(line).contains("Chunk ID"))
        );
        assert!(
            !lines
                .iter()
                .any(|line| line_text(line).contains("Wall time"))
        );
        assert!(!lines.iter().any(|line| line_text(line).contains("Output:")));
    }

    #[test]
    fn transcript_tool_output_renders_ansi_sgr_as_spans() {
        let lines = transcript_entry_lines(freeform_tool_output(
            "Chunk ID: chunk-1\nWall time: 0.123 seconds\nProcess exited with code 0\nOutput:\nplain \u{1b}[1mbright\u{1b}[0m \u{1b}[31mred\u{1b}[0m done",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ plain bright red done");
        let bright_span = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "bright")
            .expect("bright span");
        assert!(bright_span.style.add_modifier.is_empty());
        let red_span = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "red")
            .expect("red span");
        assert_eq!(red_span.style.fg, Some(Color::Red));
        let reset_span = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == " done")
            .expect("reset span");
        assert_eq!(reset_span.style.fg, None);
    }

    #[test]
    fn transcript_text_preserves_sgr_and_strips_terminal_controls() {
        let lines = transcript_entry_lines(text_entry(
            "assistant: plain \u{1b}[31mred\u{1b}[0m \u{1b}[2Jclear \u{1b}]0;title\u{7}done",
        ));

        assert_eq!(line_text(&lines[0]), " • plain red clear done");
        assert!(
            !line_text(&lines[0]).contains('\u{1b}'),
            "rendered text must not contain ESC"
        );
        let red_span = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "red")
            .expect("red span");
        assert_eq!(red_span.style.fg, Some(Color::Red));
    }

    #[test]
    fn transcript_tool_output_strips_non_sgr_terminal_controls() {
        let lines = transcript_entry_lines(freeform_tool_output(
            "Chunk ID: chunk-1\nWall time: 0.123 seconds\nProcess exited with code 0\nOutput:\nbefore\u{1b}[2Jafter\u{1b}]0;title\u{7}do\r\u{8}ne",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ beforeafterdone");
        assert!(!line_text(&lines[0]).contains('\u{1b}'));
    }

    #[test]
    fn transcript_tool_output_handles_unknown_escape_before_unicode() {
        let lines = transcript_entry_lines(freeform_tool_output(
            "Chunk ID: chunk-1\nWall time: 0.123 seconds\nProcess exited with code 0\nOutput:\nbefore\u{1b}\u{fffd}after",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ before\u{fffd}after");
    }

    #[test]
    fn transcript_tool_output_handles_incomplete_charset_escape_before_unicode() {
        let lines = transcript_entry_lines(freeform_tool_output(
            "Chunk ID: chunk-1\nWall time: 0.123 seconds\nProcess exited with code 0\nOutput:\nbefore\u{1b}(\u{fffd}after",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ before\u{fffd}after");
    }

    #[test]
    fn transcript_tool_output_fallback_uses_ten_line_budget() {
        let output = (0..120)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let entry = freeform_tool_output(&output);

        let lines = transcript_entry_lines(entry.clone());

        assert_eq!(
            transcript_entry_line_count(&entry),
            MAX_TOOL_OUTPUT_DISPLAY_LINES
        );
        assert_eq!(lines.len(), MAX_TOOL_OUTPUT_DISPLAY_LINES);
        assert_eq!(line_text(&lines[0]), " ⚙ line-0");
        assert_eq!(line_text(&lines[8]), "   line-8");
        assert_eq!(line_text(&lines[9]), "   and 111 more lines");
    }

    #[test]
    fn failed_apply_patch_attempt_is_dim_and_output_is_dark_red() {
        let entries = vec![
            freeform_tool_call(
                "apply_patch",
                "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
            ),
            freeform_tool_output(
                "Invalid patch: The first line of the patch must be '*** Begin Patch'",
            ),
        ];

        let viewport = transcript_viewport(&entries, 8, 80, 0, None);

        assert_eq!(line_text(&viewport.lines[0]), " ⚙ • Added a.txt (+1 -0)");
        assert!(
            viewport.lines[0].spans[3]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(
            viewport.lines[2].spans[3].style.fg,
            Some(TRANSCRIPT_TOOL_ERROR_FG)
        );
        assert!(line_text(&viewport.lines[2]).contains("Invalid patch"));
    }

    #[test]
    fn successful_apply_patch_output_is_not_rendered() {
        let entries = vec![
            freeform_tool_call(
                "apply_patch",
                "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
            ),
            freeform_tool_output("Success. Updated the following files:\nA a.txt"),
        ];

        let viewport = transcript_viewport(&entries, 8, 80, 0, None);

        assert_eq!(viewport.total_lines, 2);
        assert!(
            viewport
                .lines
                .iter()
                .all(|line| !line_text(line).contains("Success"))
        );
    }

    #[test]
    fn successful_edit_file_output_is_not_rendered() {
        let entries = vec![
            freeform_tool_call("edit_file", "*** Edit src/lib.rs\n*** Delete 570aa 571bb\n"),
            freeform_tool_output("ok"),
        ];

        let viewport = transcript_viewport(&entries, 8, 80, 0, None);

        assert_eq!(viewport.total_lines, 2);
        assert!(
            viewport
                .lines
                .iter()
                .all(|line| !line_text(line).contains("ok"))
        );
    }

    #[test]
    fn failed_edit_file_output_is_rendered_as_error() {
        let entries = vec![
            freeform_tool_call("edit_file", "*** Edit src/lib.rs\n*** Delete 570aa 571bb\n"),
            freeform_tool_output("edit errors\n1 src/lib.rs stale anchor 570aa\n"),
        ];

        let viewport = transcript_viewport(&entries, 8, 80, 0, None);

        assert!(
            viewport
                .lines
                .iter()
                .any(|line| line_text(line).contains("edit errors"))
        );
        assert!(viewport.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.style.fg == Some(TRANSCRIPT_TOOL_ERROR_FG))
        }));
    }

    #[test]
    fn transcript_entry_highlights_fenced_rust_code() {
        let lines =
            transcript_entry_lines(text_entry("assistant: example\n```rust\nfn main() {}\n```"));

        assert_eq!(lines[0].spans[1].style.fg, Some(TRANSCRIPT_ASSISTANT_FG));
        assert_eq!(lines[0].spans[1].content, TRANSCRIPT_ASSISTANT_MARKER);
        assert_eq!(lines[0].spans[2].content, " ");
        assert_eq!(lines[0].spans[3].content, "example");
        assert_eq!(
            lines[0].spans[3].style.fg,
            Some(TRANSCRIPT_ASSISTANT_TEXT_FG)
        );
        assert_ne!(lines[2].spans.last().and_then(|span| span.style.fg), None);
    }

    #[test]
    fn transcript_viewport_defers_syntax_highlighting_for_streaming_entry() {
        let entries = vec![text_entry("assistant: ```rust\nfn main() {}\n```")];

        let streaming = transcript_viewport(&entries, 3, 80, 0, Some(0));
        let completed = transcript_viewport(&entries, 3, 80, 0, None);

        assert_eq!(line_text(&streaming.lines[1]), "   fn main() {}");
        assert_eq!(streaming.lines[1].spans[3].style.fg, Some(Color::Gray));
        assert_ne!(
            completed.lines[1]
                .spans
                .last()
                .and_then(|span| span.style.fg),
            Some(Color::Gray)
        );
    }

    #[test]
    fn syntax_highlighting_skips_pathological_long_lines() {
        let line = "x".repeat(MAX_SYNTAX_HIGHLIGHT_LINE_BYTES + 1);

        let rendered = highlight_code_line("rust", &line);

        assert_eq!(line_text(&rendered), line);
        assert_eq!(rendered.spans[0].style.fg, Some(Color::Gray));
    }

    #[test]
    fn transcript_entry_colors_inline_code_without_removing_backticks() {
        let lines = transcript_entry_lines(text_entry("assistant: run `cargo test` now"));

        assert_eq!(line_text(&lines[0]), " • run `cargo test` now");
        assert_eq!(lines[0].spans[4].content, "`");
        assert_eq!(lines[0].spans[5].content, "cargo test");
        assert_eq!(lines[0].spans[5].style.fg, Some(TRANSCRIPT_INLINE_CODE_FG));
        assert_eq!(lines[0].spans[6].content, "`");
    }
    #[test]
    fn transcript_entry_bolds_markdown_strong_without_removing_markers() {
        let lines = transcript_entry_lines(text_entry("assistant: make **this** bold"));

        assert_eq!(line_text(&lines[0]), " • make **this** bold");
        assert_eq!(lines[0].spans[4].content, "**");
        assert_eq!(lines[0].spans[5].content, "this");
        assert!(
            lines[0].spans[5]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[6].content, "**");
    }

    #[test]
    fn transcript_entry_renders_markdown_heading_bold_wysiwyg() {
        let lines = transcript_entry_lines(text_entry("assistant: # Heading"));

        assert_eq!(line_text(&lines[0]), " • # Heading");
        assert_eq!(lines[0].spans[3].content, "# Heading");
        assert!(
            lines[0].spans[3]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn transcript_terminal_output_hides_envelope() {
        let lines = transcript_entry_lines(freeform_tool_output(
            "Chunk ID: chunk-1\nWall time: 0.123 seconds\nTerminal running with ID 2\nOutput:\nok\nmore",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ ok");
        assert_eq!(line_text(&lines[1]), "   more");
    }

    #[test]
    fn transcript_terminal_output_expands_tabs_before_rendering() {
        let lines = transcript_entry_lines(freeform_tool_output(
            "Chunk ID: chunk-1\nTerminal running with ID 1\nOutput:\nleft\tright",
        ));

        assert_eq!(line_text(&lines[0]), " ⚙ left    right");
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .flat_map(|span| span.content.chars())
                .all(|character| !character.is_control())
        );
    }

    #[test]
    fn transcript_terminal_output_hides_echoed_command_from_display() {
        let entries = vec![
            freeform_tool_call(
                "terminal_write",
                "terminal: 1\ninput:\ngit status --short; git show --stat --oneline --no-renames HEAD",
            ),
            freeform_tool_output(
                "Chunk ID: chunk-1\nTerminal running with ID 1\nOutput:\ngit status --short; git show --stat --oneline --no-renames HEAD\r\n\u{1b}[?2004l\r \u{1b}[31mm\u{1b}[m input-rs\r\n\u{1b}[33m54b43db2\u{1b}[m Extract split layout planning\r\n src/tree/container.rs | 392 \u{1b}[32m+++\u{1b}[m\u{1b}[31m---\u{1b}[m\r\n\u{1b}[?2004h",
            ),
        ];
        let output_context = transcript_entry_render_context(&entries, 1);
        let lines = transcript_entry_lines_with_context(&entries[1], output_context, usize::MAX);

        assert_eq!(line_text(&lines[0]), " ⚙  m input-rs");
        assert_eq!(
            line_text(&lines[1]),
            "   54b43db2 Extract split layout planning"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line_text(line).contains("git status --short"))
        );
        let modified_span = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "m")
            .expect("modified status span");
        assert_eq!(modified_span.style.fg, Some(Color::Red));
        let commit_span = lines[1]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "54b43db2")
            .expect("commit span");
        assert_eq!(commit_span.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn transcript_terminal_file_read_output_is_not_rendered() {
        let entries = vec![
            freeform_tool_call(
                "terminal_open",
                "command:\nsed -n '1,3p' crates/harness-tui/src/lib.rs",
            ),
            freeform_tool_output(
                "Chunk ID: chunk-1\nTerminal running with ID 1\nOutput:\nraw file line 1\nraw file line 2\nraw file line 3",
            ),
        ];
        let output_context = transcript_entry_render_context(&entries, 1);
        let lines = transcript_entry_lines_with_context(&entries[1], output_context, usize::MAX);

        assert!(lines.is_empty());
    }

    #[test]
    fn transcript_terminal_search_output_is_not_rendered() {
        let entries = vec![
            freeform_tool_call("terminal_open", "command:\nrg -n \"needle\" crates"),
            freeform_tool_output(
                "Chunk ID: chunk-1\nTerminal running with ID 1\nOutput:\ncrates/a.rs:1:needle\ncrates/b.rs:2:needle",
            ),
        ];
        let output_context = transcript_entry_render_context(&entries, 1);
        let lines = transcript_entry_lines_with_context(&entries[1], output_context, usize::MAX);

        assert!(lines.is_empty());
    }

    #[test]
    fn transcript_terminal_generic_output_is_capped_at_ten_lines() {
        let entries = vec![
            freeform_tool_call("terminal_open", "command:\ncargo test"),
            freeform_tool_output(
                "Chunk ID: chunk-1\nTerminal running with ID 1\nOutput:\none\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve",
            ),
        ];
        let output_context = transcript_entry_render_context(&entries, 1);
        let lines = transcript_entry_lines_with_context(&entries[1], output_context, usize::MAX);

        assert_eq!(lines.len(), MAX_TOOL_OUTPUT_DISPLAY_LINES);
        assert_eq!(line_text(&lines[0]), " ⚙ one");
        assert_eq!(line_text(&lines[9]), "   and 3 more lines");
    }

    #[test]
    fn matching_tool_call_and_output_render_without_separator() {
        let entries = vec![
            freeform_tool_call("terminal_open", "command:\ncargo test"),
            freeform_tool_output("Chunk ID: chunk-1\nTerminal running with ID 1\nOutput:\nok"),
        ];

        let viewport = transcript_viewport(&entries, 8, 80, 0, None);

        assert_eq!(line_text(&viewport.lines[0]), " ⚙ $ cargo test");
        assert_eq!(line_text(&viewport.lines[1]), " ⚙ ok");
        assert!(viewport.lines.iter().all(|line| line_text(line) != ""));
    }

    #[test]
    fn transcript_entry_shades_input_body_text() {
        let lines = transcript_entry_lines(text_entry("developer> fix the bug"));

        assert_eq!(lines[0].spans[1].content, TRANSCRIPT_INPUT_MARKER);
        assert_eq!(lines[0].spans[1].style.fg, Some(TRANSCRIPT_DEVELOPER_FG));
        assert_eq!(lines[0].spans[2].content, " ");
        assert_eq!(lines[0].spans[3].content, "fix the bug");
        assert_eq!(lines[0].spans[3].style.fg, Some(TRANSCRIPT_INPUT_TEXT_FG));
    }

    #[test]
    fn subagent_activity_creates_and_updates_inline_transcript_frame() {
        let mut app = TuiApp::new(UiSnapshot::default());
        assert!(app.subagent_activity_frames.is_empty());
        assert!(!any_snapshot_subagent_activity_running(&app.snapshot));

        app.apply_runtime_event(RuntimeEvent::SubagentActivity {
            activity_id: "locate-1".to_string(),
            description: "Locate".to_string(),
            status: "running".to_string(),
            detail: Some("locating: destroy_data_device".to_string()),
        });

        let frame = app
            .subagent_activity_frames
            .get("locate-1")
            .expect("frame created");
        let entry_id = frame.entry_id.expect("entry created");
        assert!(any_snapshot_subagent_activity_running(&app.snapshot));
        let lines = app.transcript_view.store.entries();
        assert!(text_entry_content(lines.last().unwrap()).contains("┌─ Locate ─"));
        assert!(text_entry_content(lines.last().unwrap()).contains("│ running"));
        assert!(
            text_entry_content(lines.last().unwrap()).contains("│ locating: destroy_data_device")
        );

        app.apply_runtime_event(RuntimeEvent::SubagentActivity {
            activity_id: "locate-1".to_string(),
            description: "Locate".to_string(),
            status: "running".to_string(),
            detail: Some("inspect: read src/ifs/ipc".to_string()),
        });
        let frame = app
            .subagent_activity_frames
            .get("locate-1")
            .expect("frame retained");
        assert_eq!(frame.entry_id, Some(entry_id), "same entry reused");
        assert_eq!(app.transcript_view.store.len(), 1, "no new entry pushed");
        let lines = app.transcript_view.store.entries();
        assert!(text_entry_content(lines.last().unwrap()).contains("│ inspect: read src/ifs/ipc"));

        app.apply_runtime_event(RuntimeEvent::SubagentActivity {
            activity_id: "locate-1".to_string(),
            description: "Locate".to_string(),
            status: "completed".to_string(),
            detail: Some("locate completed".to_string()),
        });
        assert!(!any_snapshot_subagent_activity_running(&app.snapshot));
        let frame = app
            .subagent_activity_frames
            .get("locate-1")
            .expect("final frame retained");
        assert_eq!(frame.entry_id, Some(entry_id));
        let lines = app.transcript_view.store.entries();
        assert!(text_entry_content(lines.last().unwrap()).contains("│ completed"));
        assert!(text_entry_content(lines.last().unwrap()).contains("│ locate completed"));
        assert!(!text_entry_content(lines.last().unwrap()).contains('\u{2713}'));

        app.apply_runtime_event(RuntimeEvent::SubagentActivity {
            activity_id: "sum-1".to_string(),
            description: "Summarizer".to_string(),
            status: "running".to_string(),
            detail: Some("Sent request for 'grep'".to_string()),
        });
        assert_eq!(
            app.transcript_view.store.len(),
            2,
            "different activity gets its own frame"
        );
        assert!(
            text_entry_content(app.transcript_view.store.entries().last().unwrap())
                .contains("┌─ Summarizer ─")
        );
    }
}
