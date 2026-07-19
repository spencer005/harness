//! Rewrite-owned domain values.
//!
//! Types in this module contain no `harness-core`, Crossterm, Ratatui, or
//! terminal backend values. `ExternalText` identifies text that still requires
//! display validation.

use std::collections::BTreeMap;

/// Text received from outside the TUI trust boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExternalText(String);

impl ExternalText {
    /// Wraps untrusted text without granting display safety.
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// Returns the untrusted text for validation or protocol processing.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and returns its string.
    pub(crate) fn into_string(self) -> String {
        self.0
    }

    /// Appends another untrusted fragment without granting display safety.
    pub(crate) fn append(&mut self, text: &ExternalText) {
        self.0.push_str(text.as_str());
    }
}

/// Message role represented by a transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageRole {
    /// User input.
    User,
    /// Developer-role input.
    Developer,
    /// Assistant output.
    Assistant,
}

/// Invocation encoding used by a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolInvocationKind {
    /// Freeform tool input.
    Freeform,
    /// JSON/function arguments.
    Function,
}

/// Structured display data for a tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolOutputDisplay {
    /// Source snippets returned by the inspect tool.
    InspectRead(Vec<InspectReadDisplay>),
}

/// One source snippet returned by an inspect read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InspectReadDisplay {
    /// Untrusted displayed path.
    pub(crate) path: ExternalText,
    /// First source line represented by `lines`.
    pub(crate) start_line: usize,
    /// Untrusted source lines.
    pub(crate) lines: Vec<ExternalText>,
    /// Optional continuation range.
    pub(crate) next: Option<InspectReadNext>,
}

/// Continuation location for an inspect read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InspectReadNext {
    /// First unread source line.
    pub(crate) start_line: usize,
    /// Suggested continuation line count.
    pub(crate) line_count: usize,
}

/// Encoding and structured presentation data for a tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolOutputKind {
    /// Freeform output with optional structured transcript display.
    Freeform {
        /// Structured display supplied by the runtime.
        display: Option<ToolOutputDisplay>,
    },
    /// JSON/function output.
    Function,
}

/// Semantic transcript payload independent of terminal presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptPayload {
    /// Typed conversation message.
    Message {
        /// Message role.
        role: MessageRole,
        /// Untrusted message body.
        text: ExternalText,
    },
    /// Legacy or runtime-generated plain transcript text.
    PlainText(ExternalText),
    /// Native tool invocation.
    ToolCall {
        /// Runtime correlation identifier.
        call_id: ExternalText,
        /// Untrusted tool name.
        name: ExternalText,
        /// Untrusted tool input.
        input: ExternalText,
        /// Invocation encoding.
        kind: ToolInvocationKind,
    },
    /// Native tool result.
    ToolOutput {
        /// Runtime correlation identifier.
        call_id: ExternalText,
        /// Output returned to the model.
        output: ExternalText,
        /// Optional transcript-specific output.
        display_output: Option<ExternalText>,
        /// Output encoding and structured presentation data.
        kind: ToolOutputKind,
    },
    /// Typed session-close event.
    SessionClosed {
        /// Session close time as Unix milliseconds.
        closed_at_ms: u64,
    },
    /// Runtime lifecycle event.
    Event(ExternalText),
}

/// Provider implementation kind represented independently from core DTOs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderKind {
    /// Codex provider.
    Codex,
    /// Ollama Cloud provider.
    OllamaCloud,
    /// Generic Responses-compatible HTTPS provider.
    HttpsApi,
}

/// Provider transport represented independently from core DTOs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderTransport {
    /// WebSocket streaming transport.
    WebSocket,
    /// HTTPS streaming transport.
    Https,
}

impl ProviderTransport {
    /// Returns a static user-facing transport label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::WebSocket => "ws",
            Self::Https => "https",
        }
    }
}

/// Provider details rendered by the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderState {
    /// Untrusted provider display name.
    pub(crate) display_name: ExternalText,
    /// Provider implementation kind.
    pub(crate) kind: ProviderKind,
    /// Provider transport.
    pub(crate) transport: ProviderTransport,
}

/// Model settings rendered by the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelState {
    /// Untrusted model identifier.
    pub(crate) model: ExternalText,
    /// Optional untrusted reasoning effort value.
    pub(crate) reasoning_effort: Option<ExternalText>,
    /// Optional untrusted service tier value.
    pub(crate) service_tier: Option<ExternalText>,
}

/// Context-window usage rendered by the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextUsage {
    /// Estimated current input tokens.
    pub(crate) estimated_input_tokens: u64,
    /// Maximum accepted input tokens.
    pub(crate) max_input_tokens: u64,
    /// Threshold that triggers compaction.
    pub(crate) compact_at_tokens: u64,
}

impl ContextUsage {
    /// Returns whether the usage needs warning presentation.
    pub(crate) fn needs_warning(self) -> bool {
        self.estimated_input_tokens >= self.compact_at_tokens
            || self.estimated_input_tokens > self.max_input_tokens
    }
}

/// Stable subagent identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AgentId(pub(crate) u64);

/// Subagent lifecycle represented independently from core DTOs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentStatus {
    /// Agent is actively running.
    Running,
    /// Agent is waiting for work or input.
    Waiting,
    /// Agent completed with a final message.
    Completed(ExternalText),
    /// Agent failed with a final message.
    Failed(ExternalText),
    /// Agent was interrupted.
    Interrupted,
}

/// Renderable subagent state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentState {
    /// Stable agent identifier.
    pub(crate) id: AgentId,
    /// Untrusted path or display name.
    pub(crate) path: ExternalText,
    /// Current status.
    pub(crate) status: AgentStatus,
    /// Most recent task-level message.
    pub(crate) last_task_message: Option<ExternalText>,
    /// Most recent activity message.
    pub(crate) last_activity_message: Option<ExternalText>,
}

/// Typed activity lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityStatus {
    /// Activity is in progress.
    Running,
    /// Activity completed successfully.
    Completed,
    /// Activity failed.
    Failed,
}

/// One background activity projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActivityState {
    /// Stable runtime activity identifier.
    pub(crate) id: ExternalText,
    /// Untrusted short description.
    pub(crate) description: ExternalText,
    /// Typed lifecycle status.
    pub(crate) status: ActivityStatus,
    /// Optional untrusted detail.
    pub(crate) detail: Option<ExternalText>,
}

/// Initial application values imported at the runtime boundary.
#[derive(Debug)]
pub(crate) struct InitialState {
    /// Stable session identifier.
    pub(crate) session_id: ExternalText,
    /// Human-readable title.
    pub(crate) thread_title: ExternalText,
    /// Current provider.
    pub(crate) provider: Option<ProviderState>,
    /// Current model settings.
    pub(crate) model: ModelState,
    /// Whether prompt submission uses the developer role.
    pub(crate) developer_mode: bool,
    /// Whether an assistant stream is active.
    pub(crate) response_streaming: bool,
    /// Last time-to-first-token value.
    pub(crate) last_ttft_ms: Option<u64>,
    /// Transcript state with persisted sequence identities when known.
    pub(crate) transcript: Vec<TranscriptSnapshotEntry>,
    /// Initial prompt text.
    pub(crate) prompt: String,
    /// Initial prompt cursor byte position.
    pub(crate) prompt_cursor: usize,
    /// Steering queued by the runtime.
    pub(crate) queued_steering: Option<ExternalText>,
    /// Initial subagent summaries.
    pub(crate) agents: Vec<AgentState>,
    /// Runtime activity IDs known to be active.
    pub(crate) active_activity_ids: Vec<ExternalText>,
}

/// Final application values exported at the runtime boundary.
#[derive(Debug)]
pub(crate) struct FinalState {
    /// Stable session identifier.
    pub(crate) session_id: ExternalText,
    /// Human-readable title.
    pub(crate) thread_title: ExternalText,
    /// Current provider.
    pub(crate) provider: Option<ProviderState>,
    /// Current model settings.
    pub(crate) model: ModelState,
    /// Whether prompt submission uses the developer role.
    pub(crate) developer_mode: bool,
    /// Whether an assistant stream remains active.
    pub(crate) response_streaming: bool,
    /// Last time-to-first-token value.
    pub(crate) last_ttft_ms: Option<u64>,
    /// Final transcript state with persisted sequence identities when known.
    pub(crate) transcript: Vec<TranscriptSnapshotEntry>,
    /// Final prompt text.
    pub(crate) prompt: String,
    /// Final prompt cursor.
    pub(crate) prompt_cursor: usize,
    /// Steering queue acknowledged by the runtime.
    pub(crate) queued_steering: Option<ExternalText>,
    /// Final subagent summaries.
    pub(crate) agents: Vec<AgentState>,
    /// IDs of activities still running.
    pub(crate) active_activity_ids: Vec<ExternalText>,
}

/// One semantic transcript entry at the snapshot boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptSnapshotEntry {
    /// Persisted sequence identity when known.
    pub(crate) sequence: Option<u64>,
    /// Semantic transcript payload.
    pub(crate) payload: TranscriptPayload,
}

/// One sequence-numbered persisted transcript payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedTranscriptEntry {
    /// Monotonic session record sequence.
    pub(crate) sequence: u64,
    /// Semantic transcript payload.
    pub(crate) payload: TranscriptPayload,
}

/// Runtime event after exhaustive boundary adaptation.
#[derive(Debug, Clone)]
pub(crate) enum DomainEvent {
    /// One transcript payload was appended.
    AppendTranscript(TranscriptPayload),
    /// One page of older transcript entries was loaded.
    TranscriptPage {
        /// Sequence-numbered displayable entries.
        entries: Vec<PersistedTranscriptEntry>,
        /// Cursor for the next older request.
        next_before_sequence: Option<u64>,
        /// Whether the oldest persisted entry was reached.
        reached_start: bool,
    },
    /// Model settings changed.
    ModelChanged(ModelState),
    /// Provider changed.
    ProviderChanged(ProviderState),
    /// Context-window usage changed.
    ContextUsage(ContextUsage),
    /// Root agentic work started.
    AgenticLoopStarted,
    /// Root agentic work completed.
    AgenticLoopCompleted,
    /// Prompt role routing changed.
    DeveloperModeChanged(bool),
    /// Assistant streaming started.
    ResponseStreamStarted,
    /// First-token timing was observed.
    AssistantFirstToken(u64),
    /// Assistant text was appended to the active stream.
    AssistantTextDelta(ExternalText),
    /// Assistant streaming completed.
    ResponseStreamCompleted,
    /// A subagent changed.
    AgentUpdated(AgentState),
    /// A subagent was removed.
    AgentRemoved(AgentId),
    /// A compaction operation completed.
    CompactionCompleted(ExternalText),
    /// Runtime steering queue state changed.
    SteeringChanged(Option<ExternalText>),
    /// A subagent mailbox changed.
    AgentMailboxChanged(AgentId),
    /// A background activity changed.
    ActivityChanged(ActivityState),
    /// A low-level response event was observed but has no direct UI projection.
    LowLevelResponseObserved,
    /// A malformed external event was rejected.
    ProtocolViolation(ExternalText),
    /// Runtime shutdown completed.
    ShutdownCompleted,
}

/// Runtime operation requested by application state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeRequest {
    /// Submit a new user or developer prompt.
    SubmitInput { text: String },
    /// Queue steering for the current turn.
    QueueSteering { text: String },
    /// Apply steering immediately.
    ApplySteering { text: String },
    /// Load the next older persisted transcript page.
    LoadTranscriptPage {
        /// Load entries with sequence numbers below this cursor.
        before_sequence: Option<u64>,
    },
    /// Shut down the runtime.
    Shutdown,
}

/// Session and activity values owned by the application.
#[derive(Debug)]
pub(crate) struct SessionState {
    /// Stable session identifier.
    pub(crate) session_id: ExternalText,
    /// Human-readable title.
    pub(crate) thread_title: ExternalText,
    /// Current provider.
    pub(crate) provider: Option<ProviderState>,
    /// Current model settings.
    pub(crate) model: ModelState,
    /// Whether prompt submission uses the developer role.
    pub(crate) developer_mode: bool,
    /// Whether the runtime reports an active response stream.
    pub(crate) response_streaming: bool,
    /// Last time-to-first-token value.
    pub(crate) last_ttft_ms: Option<u64>,
    /// Current context-window usage.
    pub(crate) context_usage: Option<ContextUsage>,
    /// Whether root agentic work is active.
    pub(crate) agentic_loop_working: bool,
    /// Steering text acknowledged by the runtime.
    pub(crate) queued_steering: Option<ExternalText>,
    /// Subagent summaries keyed by stable ID.
    pub(crate) agents: BTreeMap<AgentId, AgentState>,
    /// Background activities keyed by their raw ID text.
    pub(crate) activities: BTreeMap<String, ActivityState>,
}

impl SessionState {
    /// Constructs session state from imported startup values.
    pub(crate) fn from_initial(initial: &mut InitialState) -> Self {
        let agents = std::mem::take(&mut initial.agents)
            .into_iter()
            .map(|agent| (agent.id, agent))
            .collect();
        let mut activities = BTreeMap::new();
        for id in std::mem::take(&mut initial.active_activity_ids) {
            activities.insert(
                id.as_str().to_string(),
                ActivityState {
                    description: ExternalText::new("background activity"),
                    status: ActivityStatus::Running,
                    detail: None,
                    id,
                },
            );
        }
        Self {
            session_id: std::mem::take(&mut initial.session_id),
            thread_title: std::mem::take(&mut initial.thread_title),
            provider: initial.provider.take(),
            model: std::mem::replace(
                &mut initial.model,
                ModelState {
                    model: ExternalText::default(),
                    reasoning_effort: None,
                    service_tier: None,
                },
            ),
            developer_mode: initial.developer_mode,
            response_streaming: initial.response_streaming,
            last_ttft_ms: initial.last_ttft_ms,
            context_usage: None,
            agentic_loop_working: false,
            queued_steering: initial.queued_steering.take(),
            agents,
            activities,
        }
    }
}
