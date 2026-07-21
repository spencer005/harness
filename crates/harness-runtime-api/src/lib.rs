//! Frontend/runtime contracts and bounded command/event channels.

use crossfire::{AsyncRx, MAsyncTx, TrySendError, mpsc::Array};
use harness_model_api::{ContextLimits, ModelCapabilities, ModelSelection, ModelTerminalOutcome};
use harness_tool_api::ToolInput;

/// Error returned when a runtime channel cannot accept a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSendError {
    /// The receiver side of the channel is closed.
    Closed,
    /// The channel is currently full.
    Full,
}

/// Error returned when the runtime channel is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeClosed;

impl std::fmt::Display for RuntimeClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime channel closed")
    }
}

impl std::error::Error for RuntimeClosed {}

/// Asynchronous sender used for runtime command and event mailboxes.
pub type RuntimeSender<T> = MAsyncTx<Array<T>>;
/// Asynchronous receiver used for runtime command and event mailboxes.
pub type RuntimeReceiver<T> = AsyncRx<Array<T>>;

/// Command sent from a frontend to the conversation runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCommand {
    /// Submits exact prompt source.
    SubmitPrompt { text: String },
    /// Queues steering text for a later attempt.
    QueueSteering { text: String },
    /// Retries the current turn when its last durable message is retryable.
    Retry,
    /// Changes dynamic availability for tools matching a glob pattern.
    SetToolAvailability { pattern: String, enabled: bool },
    /// Starts compaction using optional user instructions.
    Compact { instruction: String },
    /// Redoes the latest staged compaction from its original source.
    RetryCompaction { instruction: Option<String> },
    /// Cancels staged compaction without changing active history.
    CancelCompaction,
    /// Stops the current agentic/request loop after the active response boundary.
    StopRequestLoop,
    /// Aborts the active model response immediately.
    AbortResponse,
    /// Interrupts the active attempt with exact user text.
    Interrupt { text: String },
    /// Changes the selected model.
    SetModel { selection: ModelSelection },
    /// Requests older persisted transcript entries.
    LoadOlderTranscript { before_sequence: Option<u64> },
    /// Sets or clears a persisted goal that keeps the agent loop running.
    SetGoal { instruction: String },
    /// Requests joined runtime shutdown.
    Shutdown,
}

/// Runtime event delivered to a frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    /// A transcript entry is appended.
    TranscriptAppended(TranscriptSnapshotEntry),
    /// A transcript page is loaded.
    TranscriptPageLoaded(TranscriptPage),
    /// Persisted sequences are assigned to the active streamed entries.
    TranscriptCommitted {
        /// Sequence assigned to the reasoning entry, when reasoning is persisted.
        reasoning_sequence: Option<u64>,
        /// Sequence assigned to the assistant message entry.
        assistant_sequence: u64,
    },
    /// The provider changes.
    ProviderChanged(ProviderSummary),
    /// The model changes.
    ModelChanged(ModelSummary),
    /// An agent changes.
    AgentChanged(AgentSummary),
    /// An agent was removed from the registry.
    AgentRemoved(u64),
    /// A background activity changes.
    ActivityChanged(Activity),
    /// A model request is waiting for the provider to begin streaming.
    ModelAwaiting(bool),
    /// Model response streaming starts.
    ResponseStarted,
    /// Assistant text arrives incrementally.
    AssistantTextDelta(String),
    /// Reasoning summary text arrives incrementally.
    ReasoningSummaryDelta(String),
    /// Raw reasoning content arrives incrementally.
    ReasoningContentDelta(String),
    /// Model response reaches a terminal outcome.
    ResponseFinished(ModelTerminalOutcome),
    /// Compaction has started.
    CompactionStarted,
    /// A compaction summary was durably committed.
    CompactionCompleted(String),
    /// Estimated context-window token usage.
    ContextUsage(ContextUsage),
    /// The root agentic work cycle started.
    AgenticLoopStarted,
    /// The root agentic work cycle completed.
    AgenticLoopCompleted,
    /// Root developer-mode routing changed.
    DeveloperModeChanged(bool),
    /// First assistant token timing observation.
    AssistantFirstToken(u64),
    /// Runtime steering queue state changed.
    SteeringChanged(Option<String>),
    /// Runtime reports a typed failure.
    Failure(RuntimeFailure),
    /// Runtime acknowledges joined shutdown.
    ShutdownComplete,
}

/// Frontend-facing context-window token usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    /// Estimated current input tokens.
    pub estimated_input_tokens: u64,
    /// Maximum accepted input tokens.
    pub max_input_tokens: u64,
    /// Threshold that triggers compaction.
    pub compact_at_tokens: u64,
}

/// Persisted transcript entry with stable identity.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSnapshotEntry {
    /// Persisted sequence, when an entry comes from storage.
    pub sequence: Option<u64>,
    /// Transcript payload.
    pub payload: TranscriptPayload,
}

/// Transcript payload represented by the frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptPayload {
    /// Message payload.
    Message { role: MessageRole, text: String },
    /// Tool call payload.
    ToolCall {
        /// Correlation identifier.
        call_id: String,
        /// Tool name.
        name: String,
        /// Input.
        input: ToolInput,
    },
    /// Tool result payload.
    ToolResult {
        /// Correlation identifier.
        call_id: String,
        /// Model-visible output.
        output: String,
    },
    /// Plain text payload.
    PlainText(String),
    /// Reasoning content or summary shown as a thinking block.
    Thinking { text: String },
    /// Turn-local provider, protocol, or execution failure.
    Error {
        /// Failure category.
        category: RuntimeFailureCategory,
        /// Original failure detail.
        message: String,
    },
}

/// Transcript message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// Developer message.
    Developer,
    /// User message.
    User,
    /// Assistant message.
    Assistant,
    /// Tool result message.
    Tool,
}

/// A page of persisted transcript entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptPage {
    /// Page entries.
    pub entries: Vec<TranscriptSnapshotEntry>,
    /// Cursor for the next older page.
    pub next_before_sequence: Option<u64>,
    /// Whether the page reaches the beginning.
    pub reached_start: bool,
}

/// Provider state shown by a frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSummary {
    /// Provider identifier.
    pub provider: String,
    /// User-facing provider name.
    pub display_name: String,
    /// Transport label.
    pub transport: String,
}

/// Model state shown by a frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSummary {
    /// Selected model.
    pub selection: ModelSelection,
    /// Model capabilities.
    pub capabilities: ModelCapabilities,
    /// Context limits.
    pub context_limits: ContextLimits,
}

/// Agent state shown by a frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    /// Stable agent identifier.
    pub agent_id: u64,
    /// Agent display path.
    pub path: String,
    /// Current status.
    pub status: AgentStatus,
    /// Most recent task-level message.
    pub last_task_message: Option<String>,
    /// Most recent activity message.
    pub last_activity_message: Option<String>,
}

/// Agent lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Agent is running.
    Running,
    /// Agent is waiting.
    Waiting,
    /// Agent completes with text.
    Completed(String),
    /// Agent fails with text.
    Failed(String),
    /// Agent is interrupted.
    Interrupted,
}

/// Background activity projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Activity identifier.
    pub activity_id: String,
    /// Activity description.
    pub description: String,
    /// Activity status.
    pub status: ActivityStatus,
    /// Optional detail.
    pub detail: Option<String>,
}

/// Activity lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    /// Activity runs.
    Running,
    /// Activity completes.
    Completed,
    /// Activity fails.
    Failed,
}

/// Runtime failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFailure {
    /// Stable failure category.
    pub category: RuntimeFailureCategory,
    /// Failure detail.
    pub message: String,
}

/// Runtime failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeFailureCategory {
    /// Session storage failure.
    Session,
    /// Model failure.
    Model,
    /// Tool failure.
    Tool,
    /// Protocol violation.
    Protocol,
    /// Runtime lifecycle failure.
    Lifecycle,
    /// A frontend command is rejected before it changes runtime state.
    Command,
}

/// Envelope that gives every event a stable runtime sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEventEnvelope {
    /// Monotonic runtime event sequence.
    pub sequence: u64,
    /// Event payload.
    pub event: RuntimeEvent,
}

impl RuntimeEventEnvelope {
    /// Creates an event envelope with an explicit sequence.
    pub fn new(sequence: u64, event: RuntimeEvent) -> Self {
        Self { sequence, event }
    }
}

/// Command sender with bounded backpressure.
#[derive(Clone)]
pub struct RuntimeCommandSender {
    tx: RuntimeSender<RuntimeCommand>,
}

impl std::fmt::Debug for RuntimeCommandSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeCommandSender")
            .finish_non_exhaustive()
    }
}

impl RuntimeCommandSender {
    /// Create a command sender from a crossfire mailbox sender.
    pub fn new(tx: RuntimeSender<RuntimeCommand>) -> Self {
        Self { tx }
    }

    /// Send a command asynchronously, waiting for mailbox capacity.
    pub async fn send(&self, command: RuntimeCommand) -> Result<(), RuntimeClosed> {
        self.tx.send(command).await.map_err(|_| RuntimeClosed)
    }

    /// Try to send a command without waiting for mailbox capacity.
    pub fn try_send(&self, command: RuntimeCommand) -> Result<(), RuntimeSendError> {
        self.tx.try_send(command).map_err(|err| match err {
            TrySendError::Full(_) => RuntimeSendError::Full,
            TrySendError::Disconnected(_) => RuntimeSendError::Closed,
        })
    }
}

/// Command receiver.
#[derive(Debug)]
pub struct RuntimeCommandReceiver {
    rx: RuntimeReceiver<RuntimeCommand>,
}

impl RuntimeCommandReceiver {
    /// Create a command receiver from a crossfire mailbox receiver.
    pub fn new(rx: RuntimeReceiver<RuntimeCommand>) -> Self {
        Self { rx }
    }

    /// Receive the next command.
    pub async fn recv(&mut self) -> Result<RuntimeCommand, RuntimeClosed> {
        self.rx.recv().await.map_err(|_| RuntimeClosed)
    }

    /// Non-blocking receive: returns `Ok(Some(cmd))` if a command is buffered,
    /// `Ok(None)` if the mailbox is empty or disconnected. The caller treats
    /// disconnection the same as empty mid-turn; the outer loop detects a
    /// closed channel on its next blocking `recv`.
    pub fn try_recv(&mut self) -> Option<RuntimeCommand> {
        self.rx.try_recv().ok()
    }
}

/// Event sender with bounded backpressure.
#[derive(Clone)]
pub struct RuntimeEventSender {
    tx: RuntimeSender<RuntimeEventEnvelope>,
}

impl std::fmt::Debug for RuntimeEventSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeEventSender").finish_non_exhaustive()
    }
}

impl RuntimeEventSender {
    /// Create an event sender from a crossfire mailbox sender.
    pub fn new(tx: RuntimeSender<RuntimeEventEnvelope>) -> Self {
        Self { tx }
    }

    /// Send an event envelope asynchronously, waiting for mailbox capacity.
    pub async fn send(&self, event: RuntimeEventEnvelope) -> Result<(), RuntimeClosed> {
        self.tx.send(event).await.map_err(|_| RuntimeClosed)
    }

    /// Try to send an event envelope without waiting for mailbox capacity.
    pub fn try_send(&self, event: RuntimeEventEnvelope) -> Result<(), RuntimeSendError> {
        self.tx.try_send(event).map_err(|err| match err {
            TrySendError::Full(_) => RuntimeSendError::Full,
            TrySendError::Disconnected(_) => RuntimeSendError::Closed,
        })
    }
}

/// Event receiver.
#[derive(Debug)]
pub struct RuntimeEventReceiver {
    rx: RuntimeReceiver<RuntimeEventEnvelope>,
}

/// Create a command and event channel pair.
///
/// Returns `(command_sender, event_receiver, event_sender, command_receiver)`.
pub fn channel_pair(
    capacity: usize,
) -> (
    RuntimeCommandSender,
    RuntimeEventReceiver,
    RuntimeEventSender,
    RuntimeCommandReceiver,
) {
    let (cmd_tx, cmd_rx) = crossfire::mpsc::bounded_async(capacity);
    let (evt_tx, evt_rx) = crossfire::mpsc::bounded_async(capacity);
    (
        RuntimeCommandSender::new(cmd_tx),
        RuntimeEventReceiver::new(evt_rx),
        RuntimeEventSender::new(evt_tx),
        RuntimeCommandReceiver::new(cmd_rx),
    )
}

impl RuntimeEventReceiver {
    /// Create an event receiver from a crossfire mailbox receiver.
    pub fn new(rx: RuntimeReceiver<RuntimeEventEnvelope>) -> Self {
        Self { rx }
    }

    /// Receive the next event envelope.
    pub async fn recv(&mut self) -> Result<RuntimeEventEnvelope, RuntimeClosed> {
        self.rx.recv().await.map_err(|_| RuntimeClosed)
    }

    /// Drain a single event without waiting, or returns `None` when empty.
    pub fn try_recv(&mut self) -> Result<Option<RuntimeEventEnvelope>, RuntimeClosed> {
        match self.rx.try_recv() {
            Ok(message) => Ok(Some(message)),
            Err(crossfire::TryRecvError::Empty) => Ok(None),
            Err(crossfire::TryRecvError::Disconnected) => Err(RuntimeClosed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_send_reports_closed_receiver() {
        let (sender, receiver, _, _) = channel_pair(1);
        drop(receiver);

        assert_eq!(
            sender.send(RuntimeCommand::Shutdown).await,
            Err(RuntimeClosed)
        );
    }

    #[tokio::test]
    async fn event_send_reports_closed_receiver() {
        let (_, _, sender, receiver) = channel_pair(1);
        drop(receiver);

        assert_eq!(
            sender
                .send(RuntimeEventEnvelope::new(1, RuntimeEvent::ShutdownComplete))
                .await,
            Err(RuntimeClosed)
        );
    }

    #[tokio::test]
    async fn command_receiver_reports_closed_sender() {
        let (sender, mut receiver, _, _) = channel_pair(1);
        drop(sender);

        assert_eq!(receiver.recv().await, Err(RuntimeClosed));
    }
}
