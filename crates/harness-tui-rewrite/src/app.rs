//! Application reducer and effect commit protocol.

use std::{
    cmp::Ordering,
    time::{Duration, Instant},
};

use crate::{
    display::ClipboardText,
    domain::{
        ActivityStatus, DomainEvent, ExternalText, FinalState, InitialState, RuntimeRequest,
        SessionState,
    },
    input::{
        BoundedInput, HorizontalUnit, InputFragment, PromptCapacityError, PromptEditor,
        PromptImportError, PromptPosition, VerticalDirection,
    },
    transcript::{Transcript, TranscriptPosition, TranscriptScrollDirection},
};

const TRANSCRIPT_SELECTION_SCROLL_INTERVAL: Duration = Duration::from_millis(50);

/// Current keyboard interaction focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    /// Prompt editing receives text and navigation.
    Prompt,
    /// Transcript selection receives copy commands.
    Transcript,
}

/// Active mouse capture owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseCapture {
    /// No drag gesture is active.
    None,
    /// Prompt selection owns the current drag.
    Prompt,
    /// Transcript selection owns the current drag.
    Transcript,
    /// The transcript scrollbar owns the current drag.
    TranscriptScrollbar {
        /// Pointer offset from the top of the rendered thumb.
        thumb_offset: u16,
    },
}

/// Notice severity used by presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoticeSeverity {
    /// Informational state.
    Information,
    /// Recoverable warning.
    Warning,
    /// Protocol or runtime failure.
    Error,
}

/// Visible application notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notice {
    /// Untrusted notice text validated during presentation.
    pub(crate) text: ExternalText,
    /// Semantic severity.
    pub(crate) severity: NoticeSeverity,
}

#[derive(Debug, Clone, Copy)]
struct TranscriptSelectionScroll {
    width: u16,
    height: usize,
    direction: TranscriptScrollDirection,
    cell: usize,
    next_at: Instant,
}

/// Interaction state that spans multiple input events.
#[derive(Debug)]
struct InteractionState {
    focus: Focus,
    mouse_capture: MouseCapture,
    transcript_selection_scroll: Option<TranscriptSelectionScroll>,
    exit_armed: bool,
    shutdown_requested: bool,
    prompt_delivery_pending: bool,
}

/// User command after terminal-specific key and mouse adaptation.
#[derive(Debug)]
pub(crate) enum UserCommand {
    /// Insert exact resource-bounded prompt text.
    Insert(InputFragment<BoundedInput>),
    /// Report a terminal input fragment rejected before editor insertion.
    InputRejected(PromptCapacityError),
    /// Delete the selection or preceding grapheme.
    DeleteBackward,
    /// Delete the selection or following grapheme.
    DeleteForward,
    /// Delete the selection or preceding word.
    DeleteWordBackward,
    /// Undo one prompt edit.
    Undo,
    /// Redo one prompt edit.
    Redo,
    /// Move prompt cursor horizontally.
    MoveHorizontal {
        /// Relative direction.
        direction: Ordering,
        /// Movement unit.
        unit: HorizontalUnit,
        /// Whether selection is extended.
        selecting: bool,
    },
    /// Move prompt cursor through visual rows.
    MoveVertical {
        /// Relative direction.
        direction: VerticalDirection,
        /// Current prompt content width.
        width: u16,
        /// Whether selection is extended.
        selecting: bool,
    },
    /// Move to a logical line boundary.
    MoveLineBoundary {
        /// Whether to move to the end rather than start.
        end: bool,
        /// Whether selection is extended.
        selecting: bool,
    },
    /// Begin prompt mouse selection.
    BeginPromptSelection { position: PromptPosition },
    /// Begin transcript mouse selection.
    BeginTranscriptSelection {
        position: Option<TranscriptPosition>,
    },
    /// Continue the active mouse selection.
    DragSelection {
        /// Hit-tested prompt boundary when the pointer is in or clamped to the
        /// prompt.
        prompt_position: Option<PromptPosition>,
        /// Hit-tested transcript position.
        transcript_position: Option<TranscriptPosition>,
    },
    /// Complete the active mouse selection.
    FinishSelection {
        /// Hit-tested prompt boundary.
        prompt_position: Option<PromptPosition>,
        /// Hit-tested transcript position.
        transcript_position: Option<TranscriptPosition>,
    },
    /// Continue transcript selection while scrolling at a viewport edge.
    DragTranscriptSelectionEdge {
        /// Current transcript width.
        width: u16,
        /// Current transcript height.
        height: usize,
        /// Direction toward content beyond the viewport edge.
        direction: TranscriptScrollDirection,
        /// Terminal cell used for the newly exposed selection endpoint.
        cell: usize,
    },
    /// Begin an absolute scrollbar drag.
    BeginTranscriptScrollbarDrag {
        /// Current transcript width.
        width: u16,
        /// Current transcript height.
        height: usize,
        /// Absolute wrapped line selected by the pointer.
        top_line: usize,
        /// Pointer offset from the top of the rendered thumb.
        thumb_offset: u16,
    },
    /// Continue an absolute scrollbar drag.
    DragTranscriptScrollbar {
        /// Current transcript width.
        width: u16,
        /// Current transcript height.
        height: usize,
        /// Absolute wrapped line selected by the pointer.
        top_line: usize,
    },
    /// Complete an absolute scrollbar drag.
    FinishTranscriptScrollbar {
        /// Current transcript width.
        width: u16,
        /// Current transcript height.
        height: usize,
        /// Absolute wrapped line selected by the pointer when geometry exists.
        top_line: Option<usize>,
    },
    /// Cancel any active mouse gesture when terminal focus is lost.
    CancelMouseCapture,
    /// Submit prompt input according to current streaming state.
    Submit,
    /// Apply queued or current steering immediately.
    Interrupt,
    /// Handle the terminal interrupt key.
    InterruptKey,
    /// Scroll transcript by visual lines; positive values move toward older
    /// content.
    ScrollTranscript {
        /// Current transcript width.
        width: u16,
        /// Current transcript height.
        height: usize,
        /// Signed visual-line movement.
        lines: isize,
    },
    /// Scroll to the oldest retained transcript line.
    ScrollTranscriptTop { width: u16, height: usize },
    /// Follow the transcript tail.
    ScrollTranscriptBottom,
    /// Copy current transcript selection.
    CopyTranscriptSelection,
}

/// Application effect executed outside the reducer.
#[derive(Debug)]
pub(crate) enum AppEffect {
    /// Deliver one ordered runtime request.
    Runtime {
        /// Runtime request.
        request: RuntimeRequest,
        /// Reducer completion applied after delivery.
        completion: DeliveryCompletion,
    },
    /// Write validated text to the terminal clipboard channel.
    Clipboard(ClipboardText),
}

/// State transition associated with one runtime delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryCompletion {
    /// No local commit follows delivery.
    None,
    /// Clear the exact prompt revision accepted by the runtime.
    Prompt(crate::input::SubmissionToken),
    /// Historical page request was delivered.
    TranscriptPage,
    /// Shutdown request was delivered.
    Shutdown,
}

/// Failure while constructing application-owned state from an external snapshot.
#[derive(Debug)]
pub(crate) enum ApplicationImportError {
    /// Prompt storage or cursor state is invalid.
    Prompt(PromptImportError),
    /// Transcript storage state or history bounds are invalid.
    Transcript(crate::transcript::TranscriptError),
}

impl std::fmt::Display for ApplicationImportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prompt(error) => error.fmt(formatter),
            Self::Transcript(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ApplicationImportError {}

impl From<PromptImportError> for ApplicationImportError {
    fn from(error: PromptImportError) -> Self {
        Self::Prompt(error)
    }
}

impl From<crate::transcript::TranscriptError> for ApplicationImportError {
    fn from(error: crate::transcript::TranscriptError) -> Self {
        Self::Transcript(error)
    }
}


/// Rewrite-owned application state.
#[derive(Debug)]
pub(crate) struct Application {
    session: SessionState,
    prompt: PromptEditor,
    transcript: Transcript,
    interaction: InteractionState,
    notice: Option<Notice>,
    agentic_started_at: Option<Instant>,
    should_exit: bool,
}

impl Application {
    /// Imports startup state and validates prompt invariants.
    pub(crate) fn import(mut initial: InitialState) -> Result<Self, ApplicationImportError> {
        let prompt =
            PromptEditor::import(std::mem::take(&mut initial.prompt), initial.prompt_cursor)?;
        let transcript = Transcript::import(
            std::mem::take(&mut initial.transcript),
            initial.response_streaming,
        )?;
        let session = SessionState::from_initial(&mut initial);
        Ok(Self {
            session,
            prompt,
            transcript,
            interaction: InteractionState {
                focus: Focus::Prompt,
                mouse_capture: MouseCapture::None,
                transcript_selection_scroll: None,
                exit_armed: false,
                shutdown_requested: false,
                prompt_delivery_pending: false,
            },
            notice: None,
            agentic_started_at: None,
            should_exit: false,
        })
    }

    /// Returns immutable session state for frame preparation.
    pub(crate) fn session(&self) -> &SessionState {
        &self.session
    }

    /// Returns mutable prompt state for canonical layout preparation.
    pub(crate) fn prompt_mut(&mut self) -> &mut PromptEditor {
        &mut self.prompt
    }

    #[cfg(test)]
    /// Returns immutable prompt state for behavior assertions.
    pub(crate) fn prompt(&self) -> &PromptEditor {
        &self.prompt
    }

    /// Returns mutable transcript state for cached frame preparation.
    pub(crate) fn transcript_mut(&mut self) -> &mut Transcript {
        &mut self.transcript
    }

    /// Returns the current mouse capture owner for terminal event routing.
    pub(crate) fn mouse_capture(&self) -> MouseCapture {
        self.interaction.mouse_capture
    }

    /// Returns the current notice.
    pub(crate) fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    /// Returns whether exit confirmation is armed.
    pub(crate) fn exit_armed(&self) -> bool {
        self.interaction.exit_armed
    }

    /// Returns whether the event loop should terminate.
    pub(crate) fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Returns elapsed root work time.
    pub(crate) fn working_elapsed(&self, now: Instant) -> Option<Duration> {
        self.agentic_started_at.and_then(|started| {
            (self.session.agentic_loop_working || self.session.compaction_working)
                .then(|| now.duration_since(started))
        })
    }
    /// Returns the delay until time-derived presentation changes again.
    ///
    /// Idle application state has no visual deadline. Root work updates on whole
    /// seconds, while an active edge-selection drag advances at a short interval.
    pub(crate) fn next_visual_change_in(&self, now: Instant) -> Option<Duration> {
        let activity_delay = self.working_elapsed(now).map(|elapsed| {
            Duration::from_secs(1)
                .saturating_sub(Duration::from_nanos(u64::from(elapsed.subsec_nanos())))
        });
        let selection_delay = self
            .interaction
            .transcript_selection_scroll
            .map(|scroll| scroll.next_at.saturating_duration_since(now));
        match (activity_delay, selection_delay) {
            (Some(activity), Some(selection)) => Some(activity.min(selection)),
            (Some(activity), None) => Some(activity),
            (None, Some(selection)) => Some(selection),
            (None, None) => None,
        }
    }

    /// Applies state changes scheduled by the current visual deadline.
    pub(crate) fn handle_visual_deadline(&mut self, now: Instant) -> Vec<AppEffect> {
        let Some(scroll) = self.interaction.transcript_selection_scroll else {
            return Vec::new();
        };
        if now < scroll.next_at {
            return Vec::new();
        }
        self.transcript.scroll_selection(
            scroll.width,
            scroll.height,
            scroll.direction,
            scroll.cell,
        );
        if let Some(active_scroll) = &mut self.interaction.transcript_selection_scroll {
            active_scroll.next_at = now + TRANSCRIPT_SELECTION_SCROLL_INTERVAL;
        }
        if scroll.direction == TranscriptScrollDirection::Older {
            self.request_older_page_effect(scroll.width, scroll.height)
        } else {
            Vec::new()
        }
    }

    /// Returns whether transcript selection currently owns copy commands.
    pub(crate) fn has_transcript_selection(&self) -> bool {
        self.transcript.has_selection()
    }

    /// Reduces one user command to zero or more effects.
    pub(crate) fn handle_user_command(&mut self, command: UserCommand) -> Vec<AppEffect> {
        if !matches!(
            command,
            UserCommand::InterruptKey
                | UserCommand::CopyTranscriptSelection
                | UserCommand::ScrollTranscript { .. }
                | UserCommand::ScrollTranscriptTop { .. }
                | UserCommand::ScrollTranscriptBottom
                | UserCommand::BeginTranscriptSelection { .. }
                | UserCommand::DragSelection { .. }
                | UserCommand::FinishSelection { .. }
                | UserCommand::DragTranscriptSelectionEdge { .. }
                | UserCommand::BeginTranscriptScrollbarDrag { .. }
                | UserCommand::DragTranscriptScrollbar { .. }
                | UserCommand::FinishTranscriptScrollbar { .. }
                | UserCommand::CancelMouseCapture
        ) {
            self.interaction.exit_armed = false;
        }

        match command {
            UserCommand::Insert(fragment) => {
                if self.prompt_edit_available() {
                    self.interaction.focus = Focus::Prompt;
                    self.transcript.clear_selection();
                    if let Err(error) = self.prompt.insert(fragment) {
                        self.set_notice(error.to_string(), NoticeSeverity::Warning);
                    }
                }
            }
            UserCommand::InputRejected(error) => {
                self.set_notice(error.to_string(), NoticeSeverity::Warning);
            }
            UserCommand::DeleteBackward => {
                if self.prompt_edit_available() {
                    self.prompt.delete_backward();
                }
            }
            UserCommand::DeleteForward => {
                if self.prompt_edit_available() {
                    self.prompt.delete_forward();
                }
            }
            UserCommand::DeleteWordBackward => {
                if self.prompt_edit_available() {
                    self.prompt.delete_word_backward();
                }
            }
            UserCommand::Undo => {
                if self.prompt_edit_available() {
                    self.prompt.undo();
                }
            }
            UserCommand::Redo => {
                if self.prompt_edit_available() {
                    self.prompt.redo();
                }
            }
            UserCommand::MoveHorizontal {
                direction,
                unit,
                selecting,
            } => {
                if self.prompt_edit_available() {
                    self.prompt.move_horizontal(direction, unit, selecting);
                }
            }
            UserCommand::MoveVertical {
                direction,
                width,
                selecting,
            } => {
                if self.prompt_edit_available() {
                    self.prompt.move_vertical(width, direction, selecting);
                }
            }
            UserCommand::MoveLineBoundary { end, selecting } => {
                if self.prompt_edit_available() {
                    self.prompt.move_to_line_boundary(end, selecting);
                }
            }
            UserCommand::BeginPromptSelection { position } => {
                if self.prompt_edit_available() {
                    self.interaction.focus = Focus::Prompt;
                    self.interaction.mouse_capture = MouseCapture::Prompt;
                    self.interaction.transcript_selection_scroll = None;
                    self.transcript.clear_selection();
                    self.prompt.begin_selection_at(position);
                }
            }
            UserCommand::BeginTranscriptSelection { position } => {
                self.interaction.focus = Focus::Transcript;
                self.interaction.mouse_capture = MouseCapture::Transcript;
                self.interaction.transcript_selection_scroll = None;
                self.prompt.clear_selection();
                if let Some(position) = position {
                    self.transcript.begin_selection(position);
                } else {
                    self.transcript.clear_selection();
                }
            }
            UserCommand::DragSelection {
                prompt_position,
                transcript_position,
            } => match self.interaction.mouse_capture {
                MouseCapture::Prompt => {
                    if let Some(position) = prompt_position
                        && self.prompt_edit_available()
                    {
                        self.prompt.extend_selection_to(position);
                    }
                }
                MouseCapture::Transcript => {
                    self.interaction.transcript_selection_scroll = None;
                    if let Some(position) = transcript_position {
                        self.transcript.extend_selection(position);
                    }
                }
                MouseCapture::TranscriptScrollbar { .. } | MouseCapture::None => {}
            },
            UserCommand::FinishSelection {
                prompt_position,
                transcript_position,
            } => {
                match self.interaction.mouse_capture {
                    MouseCapture::Prompt => {
                        if let Some(position) = prompt_position
                            && self.prompt_edit_available()
                        {
                            self.prompt.extend_selection_to(position);
                        }
                    }
                    MouseCapture::Transcript => {
                        if let Some(position) = transcript_position {
                            self.transcript.extend_selection(position);
                        }
                        self.transcript.finish_selection();
                    }
                    MouseCapture::TranscriptScrollbar { .. } | MouseCapture::None => {}
                }
                self.interaction.mouse_capture = MouseCapture::None;
                self.interaction.transcript_selection_scroll = None;
            }
            UserCommand::DragTranscriptSelectionEdge {
                width,
                height,
                direction,
                cell,
            } => {
                if self.interaction.mouse_capture == MouseCapture::Transcript {
                    let now = Instant::now();
                    let should_scroll_now = self
                        .interaction
                        .transcript_selection_scroll
                        .is_none_or(|active| active.direction != direction);
                    let next_at = self
                        .interaction
                        .transcript_selection_scroll
                        .filter(|active| active.direction == direction)
                        .map_or(now + TRANSCRIPT_SELECTION_SCROLL_INTERVAL, |active| {
                            active.next_at
                        });
                    self.interaction.transcript_selection_scroll =
                        Some(TranscriptSelectionScroll {
                            width,
                            height,
                            direction,
                            cell,
                            next_at,
                        });
                    if should_scroll_now {
                        self.transcript
                            .scroll_selection(width, height, direction, cell);
                        if direction == TranscriptScrollDirection::Older {
                            return self.request_older_page_effect(width, height);
                        }
                    }
                }
            }
            UserCommand::BeginTranscriptScrollbarDrag {
                width,
                height,
                top_line,
                thumb_offset,
            } => {
                self.interaction.focus = Focus::Transcript;
                self.interaction.mouse_capture = MouseCapture::TranscriptScrollbar { thumb_offset };
                self.interaction.transcript_selection_scroll = None;
                self.transcript.scroll_to_line(width, height, top_line);
                if top_line == 0 {
                    return self.request_older_page_effect(width, height);
                }
            }
            UserCommand::DragTranscriptScrollbar {
                width,
                height,
                top_line,
            } => {
                if matches!(
                    self.interaction.mouse_capture,
                    MouseCapture::TranscriptScrollbar { .. }
                ) {
                    self.transcript.scroll_to_line(width, height, top_line);
                    if top_line == 0 {
                        return self.request_older_page_effect(width, height);
                    }
                }
            }
            UserCommand::FinishTranscriptScrollbar {
                width,
                height,
                top_line,
            } => {
                if matches!(
                    self.interaction.mouse_capture,
                    MouseCapture::TranscriptScrollbar { .. }
                ) {
                    if let Some(top_line) = top_line {
                        self.transcript.scroll_to_line(width, height, top_line);
                    }
                    self.interaction.mouse_capture = MouseCapture::None;
                    if top_line == Some(0) {
                        return self.request_older_page_effect(width, height);
                    }
                }
            }
            UserCommand::CancelMouseCapture => {
                if self.interaction.mouse_capture == MouseCapture::Transcript {
                    self.transcript.finish_selection();
                }
                self.interaction.mouse_capture = MouseCapture::None;
                self.interaction.transcript_selection_scroll = None;
            }
            UserCommand::Submit => return self.submit_prompt(),
            UserCommand::Interrupt => return self.interrupt_response(),
            UserCommand::InterruptKey => return self.handle_interrupt_key(),
            UserCommand::ScrollTranscript {
                width,
                height,
                lines,
            } => {
                self.transcript.scroll_by(width, height, lines);
                self.interaction.focus = Focus::Transcript;
                if lines > 0 {
                    return self.request_older_page_effect(width, height);
                }
            }
            UserCommand::ScrollTranscriptTop { width, height } => {
                self.transcript.scroll_to_top(width, height);
                self.interaction.focus = Focus::Transcript;
                return self.request_older_page_effect(width, height);
            }
            UserCommand::ScrollTranscriptBottom => {
                self.transcript.scroll_to_bottom();
                self.interaction.focus = Focus::Prompt;
            }
            UserCommand::CopyTranscriptSelection => {
                if let Some(text) = self.transcript.selected_text() {
                    return vec![AppEffect::Clipboard(text)];
                }
            }
        }
        Vec::new()
    }

    /// Reduces one adapted runtime event.
    pub(crate) fn apply_domain_event(&mut self, event: DomainEvent) {
        match event {
            DomainEvent::AppendTranscript(entry) => {
                if let Err(error) = self.transcript.append_snapshot(entry) {
                    self.set_notice(
                        format!("runtime transcript protocol violation: {}", error),
                        NoticeSeverity::Error,
                    );
                }
            }
            DomainEvent::TranscriptPage {
                entries,
                next_before_sequence,
                reached_start,
            } => {
                if let Err(error) = self.transcript
                    .apply_page(entries, next_before_sequence, reached_start)
                {
                    self.set_notice(
                        format!("runtime transcript protocol violation: {}", error),
                        NoticeSeverity::Error,
                    );
                }
            }
            DomainEvent::TranscriptCommitted {
                reasoning_sequence,
                assistant_sequence,
            } => {
                if let Err(error) = self.transcript
                    .reconcile_commit(reasoning_sequence, assistant_sequence)
                {
                    self.set_notice(
                        format!("runtime transcript protocol violation: {}", error),
                        NoticeSeverity::Error,
                    );
                }
            }
            DomainEvent::ModelChanged(model) => {
                self.session.model = model;
            }
            DomainEvent::ProviderChanged(provider) => {
                self.session.provider = Some(provider);
            }
            DomainEvent::ContextUsage(usage) => {
                self.session.context_usage = Some(usage);
            }
            DomainEvent::AgenticLoopStarted => {
                if !self.session.agentic_loop_working {
                    self.session.agentic_loop_working = true;
                    self.agentic_started_at = Some(Instant::now());
                }
            }
            DomainEvent::AgenticLoopCompleted => {
                self.session.agentic_loop_working = false;
                self.agentic_started_at = None;
            }
            DomainEvent::DeveloperModeChanged(enabled) => {
                self.session.developer_mode = enabled;
            }
            DomainEvent::ModelAwaiting(awaiting) => {
                self.session.model_awaiting = awaiting;
            }
            DomainEvent::ResponseStreamStarted => {
                if let Err(error) = self.transcript.begin_response_stream() {
                    self.set_notice(
                        format!("runtime transcript protocol violation: {}", error),
                        NoticeSeverity::Error,
                    );
                }
                self.session.response_streaming = true;
                self.session.model_awaiting = false;
                self.session.last_ttft_ms = None;
            }
            DomainEvent::AssistantFirstToken(milliseconds) => {
                self.session.last_ttft_ms = Some(milliseconds);
            }
            DomainEvent::AssistantTextDelta(delta) => {
                if let Err(error) = self.transcript.append_assistant_delta(delta) {
                    self.set_notice(
                        format!("runtime transcript protocol violation: {}", error),
                        NoticeSeverity::Error,
                    );
                }
            }
            DomainEvent::ThinkingDelta(delta) => {
                if let Err(error) = self.transcript.append_thinking_delta(delta) {
                    self.set_notice(
                        format!("runtime transcript protocol violation: {}", error),
                        NoticeSeverity::Error,
                    );
                }
            }
            DomainEvent::ResponseStreamCompleted => {
                if let Err(error) = self.transcript.complete_response_stream() {
                    self.set_notice(
                        format!("runtime transcript protocol violation: {}", error),
                        NoticeSeverity::Error,
                    );
                }
                self.session.response_streaming = false;
                self.session.model_awaiting = false;
            }
            DomainEvent::ResponseStreamFailed => {
                if let Err(error) = self.transcript.complete_response_stream() {
                    self.set_notice(
                        format!("runtime transcript protocol violation: {}", error),
                        NoticeSeverity::Error,
                    );
                }
                self.session.response_streaming = false;
                self.session.model_awaiting = false;
            }
            DomainEvent::AgentUpdated(agent) => {
                self.session.agents.insert(agent.id, agent);
            }
            DomainEvent::AgentRemoved(agent) => {
                self.session.agents.remove(&agent);
            }
            DomainEvent::CompactionStarted => {
                if !self.session.compaction_working && !self.session.agentic_loop_working {
                    self.agentic_started_at = Some(Instant::now());
                }
                self.session.compaction_working = true;
                self.transcript
                    .append(crate::domain::TranscriptPayload::Event(ExternalText::new(
                        "compact: in progress".to_string(),
                    )));
            }
            DomainEvent::CompactionCompleted(summary) => {
                self.session.compaction_working = false;
                if !self.session.agentic_loop_working {
                    self.agentic_started_at = None;
                }
                self.transcript
                    .append(crate::domain::TranscriptPayload::Event(ExternalText::new(
                        format!("compact: {}", summary.as_str()),
                    )));
            }
            DomainEvent::SteeringChanged(queued) => {
                self.session.queued_steering = queued;
            }

            DomainEvent::ActivityChanged(activity) => {
                let id = activity.id.as_str().to_string();
                if activity.status == ActivityStatus::Running {
                    self.session.activities.insert(id, activity);
                } else {
                    self.session.activities.remove(&id);
                    self.transcript
                        .append(crate::domain::TranscriptPayload::Event(ExternalText::new(
                            format!(
                                "{}: {}",
                                activity.description.as_str(),
                                activity
                                    .detail
                                    .as_ref()
                                    .map(ExternalText::as_str)
                                    .unwrap_or(match activity.status {
                                        ActivityStatus::Completed => "completed",
                                        ActivityStatus::Failed => "failed",
                                        ActivityStatus::Running => "running",
                                    })
                            ),
                        )));
                }
            }
            DomainEvent::Failure(message) => {
                self.set_notice(message, NoticeSeverity::Error);
            }

            DomainEvent::ShutdownCompleted => {
                self.should_exit = true;
            }
        }
    }

    /// Commits local state after ordered runtime delivery succeeds.
    pub(crate) fn delivery_accepted(&mut self, completion: DeliveryCompletion) {
        match completion {
            DeliveryCompletion::None => {}
            DeliveryCompletion::Prompt(token) => {
                self.prompt.commit_submission(token);
                self.interaction.prompt_delivery_pending = false;
            }
            DeliveryCompletion::TranscriptPage => {}
            DeliveryCompletion::Shutdown => {
                self.interaction.shutdown_requested = true;
            }
        }
    }

    /// Restores local availability after runtime delivery fails.
    pub(crate) fn delivery_failed(
        &mut self,
        completion: DeliveryCompletion,
        message: impl Into<String>,
    ) {
        match completion {
            DeliveryCompletion::None => {}
            DeliveryCompletion::Prompt(_) => {
                self.interaction.prompt_delivery_pending = false;
            }
            DeliveryCompletion::TranscriptPage => {
                self.transcript.page_delivery_failed();
            }
            DeliveryCompletion::Shutdown => {
                self.interaction.shutdown_requested = false;
            }
        }
        self.set_notice(message, NoticeSeverity::Error);
    }

    /// Handles closure of the runtime event channel.
    pub(crate) fn runtime_disconnected(&mut self) {
        self.set_notice("runtime event channel closed", NoticeSeverity::Error);
        self.should_exit = true;
    }

    /// Consumes application state for snapshot export.
    pub(crate) fn into_final_state(self) -> FinalState {
        let (prompt, prompt_cursor) = self.prompt.into_parts();
        let active_activity_ids = self
            .session
            .activities
            .values()
            .filter(|activity| activity.status == ActivityStatus::Running)
            .map(|activity| activity.id.clone())
            .collect();
        FinalState {
            session_id: self.session.session_id,
            thread_title: self.session.thread_title,
            provider: self.session.provider,
            model: self.session.model,
            developer_mode: self.session.developer_mode,
            response_streaming: self.session.response_streaming,
            last_ttft_ms: self.session.last_ttft_ms,
            transcript: self.transcript.into_snapshot_entries(),
            prompt,
            prompt_cursor,
            queued_steering: self.session.queued_steering,
            agents: self.session.agents.into_values().collect(),
            active_activity_ids,
        }
    }

    fn request_older_page_effect(&mut self, width: u16, height: usize) -> Vec<AppEffect> {
        self.transcript
            .request_older_page(width, height)
            .map(|request| {
                vec![AppEffect::Runtime {
                    request,
                    completion: DeliveryCompletion::TranscriptPage,
                }]
            })
            .unwrap_or_default()
    }

    fn submit_prompt(&mut self) -> Vec<AppEffect> {
        if self.interaction.prompt_delivery_pending {
            return Vec::new();
        }
        let Some(submission) = self.prompt.prepare_submission() else {
            return Vec::new();
        };
        let text = submission.text().to_string();
        let request = if (self.session.response_streaming || self.session.model_awaiting)
            && !text.trim_start().starts_with('/')
        {
            self.transcript.append(crate::domain::TranscriptPayload::Message {
                role: crate::domain::MessageRole::User,
                text: crate::domain::ExternalText::new(text.clone()),
            });
            RuntimeRequest::QueueSteering { text }
        } else {
            RuntimeRequest::SubmitInput { text }
        };
        self.interaction.prompt_delivery_pending = true;
        vec![AppEffect::Runtime {
            request,
            completion: DeliveryCompletion::Prompt(submission.token()),
        }]
    }

    fn interrupt_response(&mut self) -> Vec<AppEffect> {
        let model_busy = self.session.response_streaming || self.session.model_awaiting;
        if !model_busy || self.interaction.prompt_delivery_pending {
            return Vec::new();
        }

        if let Some(submission) = self.prompt.prepare_submission() {
            // Typed text present: apply it as steering and interrupt.
            self.transcript.append(crate::domain::TranscriptPayload::Event(
                crate::domain::ExternalText::new("Steering...".to_string()),
            ));
            self.interaction.prompt_delivery_pending = true;
            let text = submission.text().to_string();
            self.transcript.append(crate::domain::TranscriptPayload::Message {
                role: crate::domain::MessageRole::User,
                text: crate::domain::ExternalText::new(text.clone()),
            });
            vec![AppEffect::Runtime {
                request: RuntimeRequest::ApplySteering { text },
                completion: DeliveryCompletion::Prompt(submission.token()),
            }]
        } else if self.session.queued_steering.is_some() {
            // No new text but queued steering exists — flush it immediately.
            vec![AppEffect::Runtime {
                request: RuntimeRequest::ApplySteering {
                    text: String::new(),
                },
                completion: DeliveryCompletion::None,
            }]
        } else {
            // No text at all. Esc means: stop the response immediately.
            // Unlike Ctrl-C this does NOT use the exit_armed state machine.
            self.transcript.append(crate::domain::TranscriptPayload::Event(
                crate::domain::ExternalText::new("Stopping...".to_string()),
            ));
            if self.session.response_streaming {
                vec![AppEffect::Runtime {
                    request: RuntimeRequest::AbortResponse,
                    completion: DeliveryCompletion::None,
                }]
            } else {
                vec![AppEffect::Runtime {
                    request: RuntimeRequest::StopRequestLoop,
                    completion: DeliveryCompletion::None,
                }]
            }
        }
    }

    fn handle_interrupt_key(&mut self) -> Vec<AppEffect> {
        if !self.prompt.is_empty() && self.prompt_edit_available() {
            self.prompt.clear();
            self.interaction.exit_armed = false;
            return Vec::new();
        }
        if self.session.response_streaming && !self.interaction.prompt_delivery_pending {
            self.transcript.append(crate::domain::TranscriptPayload::Event(
                crate::domain::ExternalText::new("Cancelling...".to_string()),
            ));
            
            if self.interaction.exit_armed {
                self.interaction.exit_armed = false;
                return vec![AppEffect::Runtime {
                    request: RuntimeRequest::AbortResponse,
                    completion: DeliveryCompletion::None,
                }];
            }
            self.interaction.exit_armed = true;
            return vec![AppEffect::Runtime {
                request: RuntimeRequest::StopRequestLoop,
                completion: DeliveryCompletion::None,
            }];
        }
        if self.interaction.exit_armed && !self.interaction.shutdown_requested {
            self.interaction.shutdown_requested = true;
            self.should_exit = true;
            return vec![AppEffect::Runtime {
                request: RuntimeRequest::Shutdown,
                completion: DeliveryCompletion::Shutdown,
            }];
        }
        self.interaction.exit_armed = true;
        Vec::new()
    }

    fn prompt_edit_available(&mut self) -> bool {
        if self.interaction.prompt_delivery_pending {
            self.set_notice(
                "prompt is awaiting runtime delivery",
                NoticeSeverity::Information,
            );
            false
        } else {
            true
        }
    }



    fn set_notice(&mut self, text: impl Into<String>, severity: NoticeSeverity) {
        self.notice = Some(Notice {
            text: ExternalText::new(text),
            severity,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{MessageRole, ModelState, TranscriptPayload, TranscriptSnapshotEntry},
        input::{InputFragment, RawInput},
    };

    fn initial() -> InitialState {
        InitialState {
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
            transcript: Vec::<TranscriptSnapshotEntry>::new(),
            prompt: String::new(),
            prompt_cursor: 0,
            queued_steering: None,
            agents: Vec::new(),
            active_activity_ids: Vec::new(),
        }
    }

    fn insert(text: &str) -> UserCommand {
        UserCommand::Insert(InputFragment::<RawInput>::new(text).bound().unwrap())
    }

    #[test]
    fn prompt_submission_is_transactional_across_delivery() {
        let mut app = Application::import(initial()).unwrap();
        app.handle_user_command(insert("hello"));
        let mut effects = app.handle_user_command(UserCommand::Submit);
        let AppEffect::Runtime { completion, .. } = effects.pop().unwrap() else {
            panic!("submit produces runtime delivery");
        };
        assert_eq!(app.prompt().text(), "hello");
        assert!(app.handle_user_command(insert("x")).is_empty());
        assert_eq!(app.prompt().text(), "hello");

        app.delivery_failed(completion, "mailbox closed");
        assert_eq!(app.prompt().text(), "hello");
        let mut effects = app.handle_user_command(UserCommand::Submit);
        let AppEffect::Runtime { completion, .. } = effects.pop().unwrap() else {
            panic!("retry produces runtime delivery");
        };
        app.delivery_accepted(completion);
        assert_eq!(app.prompt().text(), "");
    }

    #[test]
    fn invalid_stream_order_is_handled_gracefully() {
        let mut app = Application::import(initial()).unwrap();
        app.apply_domain_event(DomainEvent::AssistantTextDelta(ExternalText::new("orphan")));
        assert!(app.notice().is_some());
        assert_eq!(app.transcript.entries().count(), 0);
    }

    #[test]
    fn streaming_enter_queues_non_command_input() {
        let mut state = initial();
        state.response_streaming = true;
        let mut app = Application::import(state).unwrap();
        app.handle_user_command(insert("steer"));
        let effects = app.handle_user_command(UserCommand::Submit);
        assert!(matches!(
            &effects[0],
            AppEffect::Runtime {
                request: RuntimeRequest::QueueSteering { text },
                ..
            } if text == "steer"
        ));
    }
    #[test]
    fn streaming_submission_preserves_exact_steering_text() {
        let mut state = initial();
        state.response_streaming = true;
        let mut app = Application::import(state).unwrap();
        let text = "  steer\u{1b}[31m  ";
        app.handle_user_command(insert(text));

        let effects = app.handle_user_command(UserCommand::Submit);

        assert!(matches!(
            &effects[0],
            AppEffect::Runtime {
                request: RuntimeRequest::QueueSteering { text: actual },
                ..
            } if actual == text
        ));
    }

    #[test]
    fn immediate_steering_preserves_exact_prompt_text() {
        let mut state = initial();
        state.response_streaming = true;
        let mut app = Application::import(state).unwrap();
        let text = "  interrupt\u{1b}[31m  ";
        app.handle_user_command(insert(text));

        let effects = app.handle_user_command(UserCommand::Interrupt);

        assert!(matches!(
            &effects[0],
            AppEffect::Runtime {
                request: RuntimeRequest::ApplySteering { text: actual },
                completion: DeliveryCompletion::Prompt(_),
            } if actual == text
        ));
        assert_eq!(app.prompt().text(), text);
    }
    #[test]
    fn immediate_steering_sends_only_new_draft_text() {
        let mut state = initial();
        state.response_streaming = true;
        state.queued_steering = Some(ExternalText::new("already queued"));
        let mut app = Application::import(state).unwrap();
        app.handle_user_command(insert("new steering"));

        let effects = app.handle_user_command(UserCommand::Interrupt);

        assert!(matches!(
            &effects[0],
            AppEffect::Runtime {
                request: RuntimeRequest::ApplySteering { text },
                completion: DeliveryCompletion::Prompt(_),
            } if text == "new steering"
        ));
    }

    #[test]
    fn empty_interrupt_leaves_queued_text_for_runtime_consumption() {
        let mut state = initial();
        state.response_streaming = true;
        state.queued_steering = Some(ExternalText::new("already queued"));
        let mut app = Application::import(state).unwrap();

        let effects = app.handle_user_command(UserCommand::Interrupt);

        assert!(matches!(
            &effects[0],
            AppEffect::Runtime {
                request: RuntimeRequest::ApplySteering { text },
                completion: DeliveryCompletion::None,
            } if text.is_empty()
        ));
    }
    #[test]
    fn transcript_selection_drag_scrolls_at_both_viewport_edges_until_release() {
        let mut state = initial();
        state.transcript = vec![TranscriptSnapshotEntry {
            sequence: None,
            payload: TranscriptPayload::Message {
                role: MessageRole::Assistant,
                text: ExternalText::new(
                    "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ",
                ),
            },
        }];
        let mut app = Application::import(state).unwrap();
        let width = 6;
        let height = 3;

        let viewport = app.transcript.viewport(width, height);
        let initial_top = viewport.top_line;
        assert!(initial_top >= 4);
        let start = viewport
            .position_at(height - 1, usize::from(width - 1))
            .expect("visible transcript tail is selectable");
        app.handle_user_command(UserCommand::BeginTranscriptSelection {
            position: Some(start),
        });
        app.handle_user_command(UserCommand::DragTranscriptSelectionEdge {
            width,
            height,
            direction: TranscriptScrollDirection::Older,
            cell: 0,
        });

        assert_eq!(
            app.transcript.viewport(width, height).top_line,
            initial_top - 1
        );
        assert!(app.transcript.selected_text().is_some());

        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(app.handle_visual_deadline(deadline).is_empty());
        assert_eq!(
            app.transcript.viewport(width, height).top_line,
            initial_top - 2
        );

        app.handle_user_command(UserCommand::FinishSelection {
            prompt_position: None,
            transcript_position: None,
        });
        assert_eq!(app.next_visual_change_in(deadline), None);
        assert!(
            app.handle_visual_deadline(deadline + Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(
            app.transcript.viewport(width, height).top_line,
            initial_top - 2
        );

        let viewport = app.transcript.viewport(width, height);
        let start = viewport
            .position_at(0, 0)
            .expect("visible transcript head is selectable");
        app.handle_user_command(UserCommand::BeginTranscriptSelection {
            position: Some(start),
        });
        app.handle_user_command(UserCommand::DragTranscriptSelectionEdge {
            width,
            height,
            direction: TranscriptScrollDirection::Newer,
            cell: usize::from(width - 1),
        });

        assert_eq!(
            app.transcript.viewport(width, height).top_line,
            initial_top - 1
        );
        assert!(app.transcript.selected_text().is_some());

        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(app.handle_visual_deadline(deadline).is_empty());
        assert_eq!(app.transcript.viewport(width, height).top_line, initial_top);

        app.handle_user_command(UserCommand::CancelMouseCapture);
        assert_eq!(app.next_visual_change_in(deadline), None);
    }
}
