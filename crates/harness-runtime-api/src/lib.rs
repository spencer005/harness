//! Frontend/runtime contracts and bounded command/event channels.


use harness_model_api::{
    ContextLimits, ModelCapabilities, ModelSelection, ModelTerminalOutcome,
};

use harness_tool_api::ToolInput;
use tokio::sync::mpsc;

const DEFAULT_CHANNEL_CAPACITY: usize = 64;

/// Command sent from a frontend to the conversation runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCommand {
    /// Submits exact prompt source.
    SubmitPrompt { text: String },
    /// Queues steering text for a later attempt.
    QueueSteering { text: String },
    /// Interrupts the active attempt with exact user text.
    Interrupt { text: String },
    /// Changes the selected model.
    SetModel { selection: ModelSelection },
    /// Requests older persisted transcript entries.
    LoadOlderTranscript { before_sequence: Option<u64> },
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
    /// The provider changes.
    ProviderChanged(ProviderSummary),
    /// The model changes.
    ModelChanged(ModelSummary),
    /// An agent changes.
    AgentChanged(AgentSummary),
    /// A background activity changes.
    ActivityChanged(Activity),
    /// Model response streaming starts.
    ResponseStarted,
    /// Assistant text arrives incrementally.
    AssistantTextDelta(String),
    /// Model response reaches a terminal outcome.
    ResponseFinished(ModelTerminalOutcome),

    /// Runtime reports a typed failure.
    Failure(RuntimeFailure),
    /// Runtime acknowledges joined shutdown.
    ShutdownComplete,
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
}

/// Envelope that gives every event a stable runtime sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEventEnvelope {
    /// Monotonic runtime event sequence.
    pub sequence: u64,
    /// Event payload.
    pub event: RuntimeEvent,
}

/// Error returned when a runtime channel closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeClosed;

/// Command sender with bounded backpressure.
#[derive(Debug, Clone)]
pub struct RuntimeCommandSender {
    sender: mpsc::Sender<RuntimeCommand>,
}

/// Command receiver.
#[derive(Debug)]
pub struct RuntimeCommandReceiver {
    receiver: mpsc::Receiver<RuntimeCommand>,
}

/// Event sender with bounded backpressure.
#[derive(Debug, Clone)]
pub struct RuntimeEventSender {
    sender: mpsc::Sender<RuntimeEventEnvelope>,
}

/// Event receiver.
#[derive(Debug)]
pub struct RuntimeEventReceiver {
    receiver: mpsc::Receiver<RuntimeEventEnvelope>,
}

/// Creates bounded frontend/runtime command and event channels.
pub fn channel_pair(
    capacity: usize,
) -> (
    RuntimeCommandSender,
    RuntimeCommandReceiver,
    RuntimeEventSender,
    RuntimeEventReceiver,
) {
    assert!(capacity > 0, "runtime channel capacity must be positive");
    let (command_sender, command_receiver) = mpsc::channel(capacity);
    let (event_sender, event_receiver) = mpsc::channel(capacity);
    (
        RuntimeCommandSender {
            sender: command_sender,
        },
        RuntimeCommandReceiver {
            receiver: command_receiver,
        },
        RuntimeEventSender {
            sender: event_sender,
        },
        RuntimeEventReceiver {
            receiver: event_receiver,
        },
    )
}

/// Creates channels with the standard bounded capacity.
pub fn default_channel_pair() -> (
    RuntimeCommandSender,
    RuntimeCommandReceiver,
    RuntimeEventSender,
    RuntimeEventReceiver,
) {
    channel_pair(DEFAULT_CHANNEL_CAPACITY)
}

impl RuntimeCommandSender {
    /// Sends a command, waiting when the bounded queue is full.
    pub async fn send(&self, command: RuntimeCommand) -> Result<(), RuntimeClosed> {
        self.sender.send(command).await.map_err(|_| RuntimeClosed)
    }
}

impl RuntimeCommandReceiver {
    /// Receives the next command.

    pub async fn recv(&mut self) -> Result<RuntimeCommand, RuntimeClosed> {
        self.receiver.recv().await.ok_or(RuntimeClosed)
    }
}

impl RuntimeEventSender {
    /// Sends an event envelope, waiting when the bounded queue is full.
    pub async fn send(&self, event: RuntimeEventEnvelope) -> Result<(), RuntimeClosed> {
        self.sender.send(event).await.map_err(|_| RuntimeClosed)
    }
}

impl RuntimeEventReceiver {
    /// Receives the next event envelope.
    pub async fn recv(&mut self) -> Result<RuntimeEventEnvelope, RuntimeClosed> {
        self.receiver.recv().await.ok_or(RuntimeClosed)
    }
}

impl RuntimeEventEnvelope {
    /// Creates an event envelope with an explicit sequence.
    pub fn new(sequence: u64, event: RuntimeEvent) -> Self {
        Self { sequence, event }
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
