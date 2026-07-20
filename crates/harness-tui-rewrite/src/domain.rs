//! Rewrite-owned domain values.
//!
//! Types in this module contain no `harness-core`, Crossterm, Ratatui, or
//! terminal backend values. `ExternalText` identifies text that still requires
//! display validation.

use std::collections::BTreeMap;

pub use harness_runtime_api::{MessageRole, RuntimeFailureCategory};

/// Text received from outside the TUI trust boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalText(String);

impl ExternalText {
    /// Wraps untrusted text without granting display safety.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// Returns the untrusted text for validation or protocol processing.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and returns its string.
    pub fn into_string(self) -> String {
        self.0
    }

    /// Appends another untrusted fragment without granting display safety.
    pub fn append(&mut self, text: &ExternalText) {
        self.0.push_str(text.as_str());
    }
}

/// Invocation encoding used by a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolInvocationKind {
    /// Freeform tool input.
    Freeform,
    /// JSON/function arguments.
    Function,
}

/// Structured display data for a tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutputDisplay {
    /// Source snippets returned by the inspect tool.
    InspectRead(Vec<InspectReadDisplay>),
}

/// One source snippet returned by an inspect read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectReadDisplay {
    /// Untrusted displayed path.
    pub path: ExternalText,
    /// First source line represented by `lines`.
    pub start_line: usize,
    /// Untrusted source lines.
    pub lines: Vec<ExternalText>,
    /// Optional continuation range.
    pub next: Option<InspectReadNext>,
}

/// Continuation location for an inspect read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectReadNext {
    /// First unread source line.
    pub start_line: usize,
    /// Suggested continuation line count.
    pub line_count: usize,
}

/// Encoding and structured presentation data for a tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutputKind {
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
pub enum TranscriptPayload {
    /// Typed conversation message.
    Message {
        /// Message role.
        role: MessageRole,
        /// Untrusted message body.
        text: ExternalText,
    },
    /// Legacy or runtime-generated plain transcript text.
    PlainText(ExternalText),
    /// Reasoning content or summary displayed as a thinking block.
    Thinking(ExternalText),
    /// Failure associated with the active turn.
    Error {
        /// Failure category.
        category: RuntimeFailureCategory,
        /// Original failure detail.
        message: ExternalText,
    },
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
pub enum ProviderKind {
    /// Codex provider.
    Codex,
    /// Ollama Cloud provider.
    OllamaCloud,
    /// Generic Responses-compatible HTTPS provider.
    HttpsApi,
}

/// Provider transport represented independently from core DTOs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTransport {
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
pub struct ProviderState {
    /// Untrusted provider display name.
    pub display_name: ExternalText,
    /// Provider implementation kind.
    pub kind: ProviderKind,
    /// Provider transport.
    pub transport: ProviderTransport,
}

/// Model settings rendered by the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelState {
    /// Untrusted model identifier.
    pub model: ExternalText,
    /// Optional untrusted reasoning effort value.
    pub reasoning_effort: Option<ExternalText>,
    /// Optional untrusted service tier value.
    pub service_tier: Option<ExternalText>,
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
pub struct AgentId(pub u64);

/// Subagent lifecycle represented independently from core DTOs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
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
pub struct AgentState {
    /// Stable agent identifier.
    pub id: AgentId,
    /// Untrusted path or display name.
    pub path: ExternalText,
    /// Current status.
    pub status: AgentStatus,
    /// Most recent task-level message.
    pub last_task_message: Option<ExternalText>,
    /// Most recent activity message.
    pub last_activity_message: Option<ExternalText>,
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
pub struct InitialState {
    /// Stable session identifier.
    pub session_id: ExternalText,
    /// Human-readable title.
    pub thread_title: ExternalText,
    /// Current provider.
    pub provider: Option<ProviderState>,
    /// Current model settings.
    pub model: ModelState,
    /// Whether prompt submission uses the developer role.
    pub developer_mode: bool,
    /// Whether an assistant stream remains active.
    pub response_streaming: bool,
    /// Last time-to-first-token value.
    pub last_ttft_ms: Option<u64>,
    /// Transcript state with persisted sequence identities when known.
    pub transcript: Vec<TranscriptSnapshotEntry>,
    /// Initial prompt text.
    pub prompt: String,
    /// Initial prompt cursor byte position.
    pub prompt_cursor: usize,
    /// Steering queued by the runtime.
    pub queued_steering: Option<ExternalText>,
    /// Initial subagent summaries.
    pub agents: Vec<AgentState>,
    /// Runtime activity IDs known to be active.
    pub active_activity_ids: Vec<ExternalText>,
}

/// Final application values exported at the runtime boundary.
#[derive(Debug)]
pub struct FinalState {
    /// Stable session identifier.
    pub session_id: ExternalText,
    /// Human-readable title.
    pub thread_title: ExternalText,
    /// Current provider.
    pub provider: Option<ProviderState>,
    /// Current model settings.
    pub model: ModelState,
    /// Whether prompt submission uses the developer role.
    pub developer_mode: bool,
    /// Whether an assistant stream remains active.
    pub response_streaming: bool,
    /// Last time-to-first-token value.
    pub last_ttft_ms: Option<u64>,
    /// Final transcript state with persisted sequence identities when known.
    pub transcript: Vec<TranscriptSnapshotEntry>,
    /// Final prompt text.
    pub prompt: String,
    /// Final prompt cursor.
    pub prompt_cursor: usize,
    /// Steering queue acknowledged by the runtime.
    pub queued_steering: Option<ExternalText>,
    /// Final subagent summaries.
    pub agents: Vec<AgentState>,
    /// IDs of activities still running.
    pub active_activity_ids: Vec<ExternalText>,
}

/// One semantic transcript entry at the snapshot boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSnapshotEntry {
    /// Persisted sequence identity when known.
    pub sequence: Option<u64>,
    /// Semantic transcript payload.
    pub payload: TranscriptPayload,
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
    /// One transcript entry was appended.
    AppendTranscript(TranscriptSnapshotEntry),
    /// One page of older transcript entries was loaded.
    TranscriptPage {
        /// Sequence-numbered displayable entries.
        entries: Vec<PersistedTranscriptEntry>,
        /// Cursor for the next older request.
        next_before_sequence: Option<u64>,
        /// Whether the oldest persisted entry was reached.
        reached_start: bool,
    },
    /// Persisted sequences are assigned to active streamed entries.
    TranscriptCommitted {
        /// Sequence assigned to the reasoning entry, when present.
        reasoning_sequence: Option<u64>,
        /// Sequence assigned to the assistant message entry.
        assistant_sequence: u64,
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
    /// A model request is waiting for the provider.
    ModelAwaiting(bool),
    /// Assistant streaming started.
    ResponseStreamStarted,
    /// First-token timing was observed.
    AssistantFirstToken(u64),
    /// Assistant text was appended to the active stream.
    AssistantTextDelta(ExternalText),
    /// Reasoning text was appended to the active thinking block.
    ThinkingDelta(ExternalText),
    /// Assistant streaming completed.
    ResponseStreamCompleted,
    /// Assistant streaming failed after the runtime persisted the outcome.
    ResponseStreamFailed,
    /// A subagent changed.
    AgentUpdated(AgentState),
    /// A subagent was removed.
    AgentRemoved(AgentId),
    /// A compaction operation completed.
    CompactionCompleted(ExternalText),
    /// Runtime steering queue state changed.
    SteeringChanged(Option<ExternalText>),
    /// A background activity changed.
    ActivityChanged(ActivityState),
    /// A runtime failure surfaced to the frontend.
    Failure(String),
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
    /// Retry the latest durable user or tool turn.
    Retry,
    /// Change availability for tools matching a glob pattern.
    SetToolAvailability { pattern: String, enabled: bool },
    /// Start compaction with optional instructions.
    Compact { instruction: String },
    /// Redo staged compaction from the original source.
    RetryCompaction { instruction: Option<String> },
    /// Cancel staged compaction.
    CancelCompaction,
    /// Stop the current agentic/request loop after the active boundary.
    StopRequestLoop,
    /// Abort the active model response immediately.
    AbortResponse,
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
    /// Whether a model request is awaiting provider output.
    pub(crate) model_awaiting: bool,
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
            model_awaiting: false,
            last_ttft_ms: initial.last_ttft_ms,
            context_usage: None,
            agentic_loop_working: false,
            queued_steering: initial.queued_steering.take(),
            agents,
            activities,
        }
    }
}
