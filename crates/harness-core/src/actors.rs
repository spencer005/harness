use std::{fmt, marker::Unpin};

use crossfire::{AsyncRx, MAsyncTx, TrySendError, mpsc::Array};
use thiserror::Error;

use crate::{
    compact::{CompactRequest, CompactResult, ContextWindowUsage},
    responses::{ModelSettings, ResponsesEvent, ResponsesRequest},
    sessions::TranscriptPage,
    steering::SteeringMode,
    subagents::{AgentId, AgentSummary},
};

/// Default bounded mailbox capacity used by harness actors.
pub const DEFAULT_ACTOR_MAILBOX_CAPACITY: usize = 1024;

/// Asynchronous sender used for actor command and event mailboxes.
pub type ActorSender<T> = MAsyncTx<Array<T>>;
/// Asynchronous receiver used for actor command and event mailboxes.
pub type ActorReceiver<T> = AsyncRx<Array<T>>;

/// Error returned when an actor mailbox cannot accept a message.
#[derive(Debug, Error)]
pub enum ActorSendError {
    /// The receiver side of the actor mailbox is closed.
    #[error("actor mailbox closed")]
    Closed,
    /// The actor mailbox is currently full.
    #[error("actor mailbox full")]
    Full,
}

/// Cloneable handle for sending commands to an actor.
#[derive(Clone)]
pub struct ActorHandle<T: Unpin + 'static> {
    tx: ActorSender<T>,
}

impl<T: Unpin + 'static> ActorHandle<T> {
    /// Create a handle from an actor mailbox sender.
    pub fn new(tx: ActorSender<T>) -> Self {
        Self { tx }
    }

    /// Send a message asynchronously, waiting for mailbox capacity.
    pub async fn send(&self, message: T) -> Result<(), ActorSendError> {
        self.tx
            .send(message)
            .await
            .map_err(|_| ActorSendError::Closed)
    }

    /// Try to send a message without waiting for mailbox capacity.
    pub fn try_send(&self, message: T) -> Result<(), ActorSendError> {
        self.tx.try_send(message).map_err(|err| match err {
            TrySendError::Full(_) => ActorSendError::Full,
            TrySendError::Disconnected(_) => ActorSendError::Closed,
        })
    }

    /// Return a clone of the underlying mailbox sender.
    pub fn sender(&self) -> ActorSender<T> {
        self.tx.clone()
    }
}

impl<T: Unpin + 'static> fmt::Debug for ActorHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActorHandle").finish_non_exhaustive()
    }
}

/// Commands accepted by the root harness runtime.
#[derive(Debug)]
pub enum RuntimeCommand {
    /// Submit a prompt to the active session.
    SubmitInput {
        /// Prompt text submitted by the user.
        text: String,
    },
    /// Replace the active model settings.
    SetModelSettings {
        /// Model settings used for subsequent requests.
        settings: ModelSettings,
    },
    /// Queue steering text for the next tool-call boundary.
    QueueSteering {
        /// Steering text to queue.
        text: String,
    },
    /// Apply steering using the requested delivery mode.
    ApplySteering {
        /// Steering text to apply.
        text: String,
        /// Delivery mode for the steering text.
        mode: SteeringMode,
    },
    /// Submit a raw Responses request.
    SubmitResponsesRequest {
        /// Responses request to submit.
        request: ResponsesRequest,
    },
    /// Load an older transcript page.
    LoadTranscriptPage {
        /// Sequence number before which older lines are requested.
        before_seq: Option<u64>,
        /// Maximum number of transcript lines to load.
        max_lines: usize,
    },
    /// Run a compaction request.
    Compact {
        /// Compaction request to submit.
        request: CompactRequest,
    },
    /// Shut down the runtime.
    Shutdown,
}

/// Events emitted by the root harness runtime.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// One plain text transcript entry was appended.
    TranscriptLine(String),
    /// One freeform tool-call transcript entry was appended.
    FreeformToolCall(crate::sessions::FreeformToolCallRecord),
    /// One freeform tool-output transcript entry was appended.
    FreeformToolOutput(crate::sessions::FreeformToolOutputRecord),
    /// One function tool-call transcript entry was appended.
    FunctionToolCall(crate::sessions::FunctionToolCallRecord),
    /// One function tool-output transcript entry was appended.
    FunctionToolOutput(crate::sessions::FunctionToolOutputRecord),
    /// Older transcript lines were loaded from storage.
    TranscriptPage(TranscriptPage),
    /// Model settings changed.
    ModelSettingsChanged(ModelSettings),
    /// Provider profile changed.
    ProviderChanged(crate::providers::ProviderUiInfo),
    /// Current context-window token usage estimate.
    ContextWindowUsage(ContextWindowUsage),
    /// The root agentic loop started.
    AgenticLoopStarted,
    /// The root agentic loop completed.
    AgenticLoopCompleted,
    /// Developer-mode input routing changed.
    DeveloperModeChanged(bool),
    /// A response stream started.
    ResponseStreamStarted,
    /// First assistant token timing was observed.
    AssistantFirstToken {
        /// Time to first token in milliseconds.
        ttft_ms: u64,
    },
    /// Assistant text delta was received.
    AssistantTextDelta(String),
    /// The active response stream completed.
    ResponseStreamCompleted,
    /// Low-level Responses actor event was emitted.
    Responses(ResponsesEvent),
    /// A subagent summary changed.
    AgentUpdated(AgentSummary),
    /// A subagent was removed from the registry.
    AgentRemoved(AgentId),
    /// A compaction request completed.
    CompactCompleted(CompactResult),
    /// Root steering queue state changed.
    SteeringQueued(Option<String>),
    /// A subagent mailbox received a queued message.
    AgentMailboxUpdate {
        /// Agent whose mailbox changed.
        agent_id: AgentId,
    },
    /// A subagent or background activity update.
    SubagentActivity {
        /// Unique activity identifier.
        activity_id: String,
        /// Short description/name of the activity.
        description: String,
        /// Current status: "running", "completed", "failed".
        status: String,
        /// Detail text (optional).
        detail: Option<String>,
    },
    /// Runtime shutdown completed.
    ShutdownComplete,
}

/// Create a bounded actor mailbox and its sending handle.
pub fn channel<T: Unpin + 'static>(capacity: usize) -> (ActorHandle<T>, ActorReceiver<T>) {
    let (tx, rx) = crossfire::mpsc::bounded_async(capacity);
    (ActorHandle::new(tx), rx)
}
