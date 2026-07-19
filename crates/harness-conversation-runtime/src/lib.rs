//! Conversation policy and lifecycle contracts above injected ports.
//!
//! This crate does not parse provider wire data, open files, resolve
//! credentials, or implement PTYs. The session store remains behind its
//! current-format adapter until the final user-run migration phase.

use std::{
    collections::BTreeMap,
    sync::Arc,
};

use harness_model_api::{
    ModelAttempt, ModelCancellation, ModelEvent, ModelFailure, ModelFailureKind, ModelInterruption,
    ModelRequest, ModelSelection, ModelTerminalOutcome, ModelTransport, ResolvedModelRoute,
};

use harness_runtime_api::{
    MessageRole, RuntimeCommand, RuntimeEvent, TranscriptPayload, TranscriptSnapshotEntry,
};
use harness_session_store::{
    AppendReceipt, Durability, PageSize, SessionPayload, SessionStore, SessionStoreError,
    SessionWriter, TranscriptPage as StoredTranscriptPage, TurnOutcome,
};
use harness_tool_api::{
    ToolExecutionId, ToolExecutionPolicy, ToolExecutionRequest, ToolExecutor, ToolFailure, ToolName,
    ToolRegistry,
};
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;


/// Lifecycle of the conversation runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLifecycle {
    /// Runtime is constructed but has not started its owned jobs.
    Constructed,
    /// Runtime is loading state and starting injected services.
    Starting,
    /// Runtime accepts commands and model events.
    Ready,
    /// Runtime is cancelling and joining owned jobs.
    ShuttingDown,
    /// Runtime has joined all owned jobs.
    Stopped,
}

/// Explicit persist-mode state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistState {
    /// Persist mode is disabled.
    Disabled,
    /// Persist mode is active.
    Active(PersistTask),
    /// Persist mode is paused.
    Paused(PersistTask),
    /// Persist mode completed explicitly.
    Completed(PersistTask),
}

/// Persist-mode instruction and completion policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistTask {
    /// Exact instruction supplied by the user.
    pub instruction: String,
    /// Explicit authority to complete the task.
    pub completion_policy: CompletionPolicy,
}

/// Completion authority for persist mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionPolicy {
    /// The model may complete the task.
    ModelMayComplete,
    /// Only explicit user/runtime control may complete the task.
    UserOnly,
}
/// Mutually exclusive root conversation phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationPhase {
    /// No turn is active.
    Idle,
    /// Durable input records are being appended.
    PersistingInput { turn_id: u64 },
    /// A model request is being assembled.
    PreparingAttempt { turn_id: u64 },
    /// The transport is starting an attempt.
    AwaitingModel { turn_id: u64, attempt_id: u64 },
    /// Assistant deltas are being received.
    Streaming { turn_id: u64, attempt_id: u64 },
    /// Assistant output is being committed.
    PersistingAssistant { turn_id: u64, attempt_id: u64 },
    /// A tool call is being committed before execution.
    PersistingToolCall { turn_id: u64, call_id: String },
    /// A tool is executing outside the reducer.
    ExecutingTool {
        turn_id: u64,
        execution_id: u64,
        call_id: String,
    },
    /// A tool result is being committed.
    PersistingToolResult {
        turn_id: u64,
        execution_id: u64,
        call_id: String,
    },
    /// Continuation request is being prepared.
    PreparingContinuation { turn_id: u64 },
    /// Active model work is being cancelled.
    Cancelling { turn_id: u64, attempt_id: u64 },
    /// A compaction job is active.
    Compacting { compaction_id: u64 },
    /// A turn or lifecycle operation failed and is returning to idle.
    Failed { turn_id: Option<u64>, message: String },
}

/// One immutable model attempt supervised by the runtime.
#[derive(Debug, Clone)]
pub struct ActiveModelAttempt {
    /// Turn owning the attempt.
    pub turn_id: u64,
    /// Immutable semantic attempt snapshot.
    pub attempt: Arc<ModelAttempt>,
    /// Provider route used for the attempt.
    pub route: ResolvedModelRoute,
    /// Current attempt phase.
    pub phase: AttemptPhase,
    /// Assistant text accumulated from deltas.
    pub assistant_text: String,
    /// Terminal outcome after the attempt reaches its terminal phase.
    pub terminal_outcome: Option<ModelTerminalOutcome>,
}

/// Lifecycle of one model attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptPhase {
    /// Attempt is created but not submitted.
    Created,
    /// Transport connects.
    Connecting,
    /// Transport streams events.
    Streaming,
    /// Terminal event is received.
    Terminal,
    /// Cancellation is requested.
    Cancelling,
}

impl ActiveModelAttempt {
    /// Applies one decoded model event to the active attempt.
    pub fn apply_event(&mut self, event: ModelEvent) -> Result<(), RuntimeError> {
        if self.phase == AttemptPhase::Terminal || self.phase == AttemptPhase::Cancelling {
            return Err(RuntimeError::InvalidPhase);
        }

        match event {
            ModelEvent::Started => {
                self.phase = AttemptPhase::Connecting;
            }
            ModelEvent::AssistantTextDelta(delta) => {
                self.phase = AttemptPhase::Streaming;
                self.assistant_text.push_str(&delta);
            }
            ModelEvent::ToolInputDelta(_) | ModelEvent::ToolCall(_) => {
                self.phase = AttemptPhase::Streaming;
            }
            ModelEvent::Metadata(_) | ModelEvent::Usage(_) => {}
            ModelEvent::Terminal(outcome) => {
                self.phase = AttemptPhase::Terminal;
                self.terminal_outcome = Some(outcome);
            }

        }
        Ok(())
    }
}

/// Purpose assigned to every supervised asynchronous job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobPurpose {
    /// Root model attempt.
    RootModelAttempt { attempt_id: u64 },
    /// Compaction.
    Compaction { compaction_id: u64 },
    /// Tool execution.
    ToolExecution { execution_id: u64 },
    /// Provider resolution.
    ProviderResolution { operation_id: u64 },
    /// Agent execution.
    AgentRun { agent_id: u64 },
}

/// Typed completion returned by a supervised job.
#[derive(Debug)]
pub enum JobCompletion {
    /// Model event from a registered attempt.
    ModelEvent {
        /// Turn identity.
        turn_id: u64,
        /// Attempt identity.
        attempt_id: u64,
        /// Provider event.
        event: ModelEvent,
    },
    /// Tool execution result.
    ToolResult {
        /// Turn identity.
        turn_id: u64,
        /// Execution identity.
        execution_id: u64,
        /// Model tool-call identity.
        call_id: String,
        /// Result or failure.
        result: Result<harness_tool_api::ToolResult, harness_tool_api::ToolFailure>,
    },
    /// A job completes after sending every payload.
    Finished {
        /// Job purpose.
        purpose: JobPurpose,
    },
}

/// Registry of every asynchronous job owned by the runtime.
#[derive(Debug, Default)]
pub struct JobRegistry {
    jobs: BTreeMap<JobPurpose, SupervisedJob>,
}

#[derive(Debug)]
struct SupervisedJob {
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
}

impl JobRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a job and rejects duplicate purposes.
    pub fn register(
        &mut self,
        purpose: JobPurpose,
        cancellation: CancellationToken,
        handle: JoinHandle<()>,
    ) -> Result<(), RuntimeError> {
        if self.jobs.contains_key(&purpose) {
            return Err(RuntimeError::DuplicateJob);
        }
        self.jobs.insert(
            purpose,
            SupervisedJob {
                cancellation,
                handle,
            },
        );
        Ok(())
    }

    /// Requests cancellation for all jobs without detaching them.
    pub fn cancel_all(&self) {
        for job in self.jobs.values() {
            job.cancellation.cancel();
        }
    }

    /// Requests cancellation for one registered job.
    pub fn cancel(&self, purpose: JobPurpose) -> Result<(), RuntimeError> {
        self.jobs
            .get(&purpose)
            .ok_or(RuntimeError::UnknownJob)?
            .cancellation
            .cancel();
        Ok(())
    }

    /// Returns whether every supervised job has been retired or joined.
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Removes a completed job exactly once.
    pub fn retire(&mut self, purpose: JobPurpose) -> Result<JoinHandle<()>, RuntimeError> {
        self.jobs
            .remove(&purpose)
            .map(|job| job.handle)
            .ok_or(RuntimeError::UnknownJob)
    }

    /// Cancels and joins every owned job.
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.cancel_all();
        let mut first_error = None;
        for (_, job) in self.jobs {
            if let Err(error) = job.handle.await {
                if first_error.is_none() {
                    first_error = Some(RuntimeError::JobJoin(error.to_string()));
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

}

/// Ports injected into the conversation runtime composition layer.
pub struct RuntimePorts {
    /// Durable session store.
    pub session_store: Arc<dyn SessionStore>,
    /// Provider-independent tool registry.
    pub tool_registry: ToolRegistry,
    /// Tool execution implementation.
    pub tool_executor: Arc<dyn ToolExecutor>,
    /// Model transport implementation.
    pub model_transport: Arc<dyn ModelTransport>,
    /// Builds immutable semantic model requests.
    pub request_builder: Arc<dyn ModelRequestBuilder>,
    /// Route selected for root model requests.
    pub model_route: ResolvedModelRoute,


}

/// Runtime construction input.
pub struct RuntimeConfiguration {
    /// Session identifier selected by the composition root.
    pub session_id: harness_session_store::SessionId,
    /// Initial model selection.
    pub model: ModelSelection,
    /// Runtime ports.
    pub ports: RuntimePorts,
}

/// Runtime handle for the reducer implementation.
pub struct ConversationRuntime {
    lifecycle: RuntimeLifecycle,
    phase: ConversationPhase,
    configuration: RuntimeConfiguration,
    jobs: JobRegistry,
    writer: Option<RuntimeSessionWriter>,
    canonical_history: Vec<SessionPayload>,
    transient_assistant: String,
    pending_tool_inputs: BTreeMap<String, String>,
    pending_tool_calls: Vec<harness_model_api::ToolCall>,
    active_tool_call: Option<harness_model_api::ToolCall>,
    next_execution_id: u64,
    persist_state: PersistState,
    queued_steering: Vec<String>,
    pending_interrupt: Option<String>,
    canonical_revision: u64,
    transport_stopped: bool,
    next_attempt_id: u64,







    next_turn_id: u64,
}



impl ConversationRuntime {
    /// Constructs a runtime before any storage, model, or tool work starts.
    pub fn new(configuration: RuntimeConfiguration) -> Self {
        Self {
            lifecycle: RuntimeLifecycle::Constructed,
            phase: ConversationPhase::Idle,
            configuration,
            jobs: JobRegistry::new(),
            writer: None,
            canonical_history: Vec::new(),
            transient_assistant: String::new(),
            pending_tool_inputs: BTreeMap::new(),
            pending_tool_calls: Vec::new(),
            active_tool_call: None,
            next_turn_id: 1,
            next_execution_id: 1,
            persist_state: PersistState::Disabled,
            queued_steering: Vec::new(),
            pending_interrupt: None,
            canonical_revision: 0,
            transport_stopped: false,
            next_attempt_id: 1,

        }
    }

    /// Acquires the unique session writer and enters startup.
    pub async fn begin_startup(&mut self) -> Result<(), RuntimeError> {
        if self.lifecycle != RuntimeLifecycle::Constructed {
            return Err(RuntimeError::InvalidLifecycle);
        }
        let writer = self
            .configuration
            .ports
            .session_store
            .writer(self.configuration.session_id.clone())
            .await?;
        self.writer = Some(RuntimeSessionWriter::new(writer));
        self.lifecycle = RuntimeLifecycle::Starting;
        Ok(())
    }


    /// Marks startup complete after composition has satisfied its readiness contract.
    pub fn mark_ready(&mut self) -> Result<(), RuntimeError> {
        if self.lifecycle != RuntimeLifecycle::Starting {
            return Err(RuntimeError::InvalidLifecycle);
        }
        self.lifecycle = RuntimeLifecycle::Ready;
        Ok(())
    }

    /// Begins cancellation and joined shutdown.
    pub fn begin_shutdown(&mut self) -> Result<(), RuntimeError> {
        if !matches!(
            self.lifecycle,
            RuntimeLifecycle::Ready | RuntimeLifecycle::Starting
        ) {
            return Err(RuntimeError::InvalidLifecycle);
        }
        self.lifecycle = RuntimeLifecycle::ShuttingDown;
        if let ConversationPhase::AwaitingModel {
            turn_id,
            attempt_id,
        }
        | ConversationPhase::Streaming {
            turn_id,
            attempt_id,
        } = &self.phase
        {
            self.phase = ConversationPhase::Cancelling {
                turn_id: *turn_id,
                attempt_id: *attempt_id,
            };
        }
        self.jobs.cancel_all();
        Ok(())
    }

    /// Joins every supervised job before the session writer closes.
    pub async fn join_jobs(&mut self) -> Result<(), RuntimeError> {
        let jobs = std::mem::take(&mut self.jobs);
        jobs.shutdown().await
    }

    /// Shuts down the model transport after runtime jobs are joined.
    pub async fn shutdown_transport(&mut self) -> Result<(), RuntimeError> {
        if self.lifecycle != RuntimeLifecycle::ShuttingDown || !self.jobs.is_empty() {
            return Err(RuntimeError::InvalidLifecycle);
        }
        self.configuration
            .ports
            .model_transport
            .shutdown()
            .await
            .map_err(RuntimeError::ModelTransport)?;
        self.transport_stopped = true;
        Ok(())
    }

    /// Closes the session writer after owned jobs and transport are joined.
    pub async fn mark_stopped(&mut self) -> Result<(), RuntimeError> {
        if self.lifecycle != RuntimeLifecycle::ShuttingDown
            || !self.jobs.is_empty()
            || !self.transport_stopped
        {
            return Err(RuntimeError::InvalidLifecycle);
        }
        let writer = self.writer.take().ok_or(RuntimeError::InvalidLifecycle)?;
        writer.close().await?;
        self.lifecycle = RuntimeLifecycle::Stopped;
        Ok(())
    }


    /// Enables persist mode with an explicit completion policy.
    pub fn enable_persist(
        &mut self,
        instruction: String,
        completion_policy: CompletionPolicy,
    ) -> Result<(), RuntimeError> {
        if self.lifecycle != RuntimeLifecycle::Ready {
            return Err(RuntimeError::InvalidLifecycle);
        }
        self.persist_state = PersistState::Active(PersistTask {
            instruction,
            completion_policy,
        });
        Ok(())
    }

    /// Pauses persist mode without losing its task or completion policy.
    pub fn pause_persist(&mut self) -> Result<(), RuntimeError> {
        let PersistState::Active(task) = &self.persist_state else {
            return Err(RuntimeError::InvalidPhase);
        };
        self.persist_state = PersistState::Paused(task.clone());
        Ok(())
    }

    /// Resumes a paused persist task.
    pub fn resume_persist(&mut self) -> Result<(), RuntimeError> {
        let PersistState::Paused(task) = &self.persist_state else {
            return Err(RuntimeError::InvalidPhase);
        };
        self.persist_state = PersistState::Active(task.clone());
        Ok(())
    }

    /// Completes persist mode through explicit runtime control.
    pub fn complete_persist(&mut self) -> Result<(), RuntimeError> {
        let task = match &self.persist_state {
            PersistState::Active(task) | PersistState::Paused(task) => task.clone(),
            _ => return Err(RuntimeError::InvalidPhase),
        };
        self.persist_state = PersistState::Completed(task);
        Ok(())
    }

    /// Persists exact prompt source before changing canonical conversation state.
    pub async fn submit_prompt(&mut self, text: String) -> Result<AppendReceipt, RuntimeError> {
        if self.lifecycle != RuntimeLifecycle::Ready {
            return Err(RuntimeError::InvalidLifecycle);
        }
        if self.phase != ConversationPhase::Idle {
            return Err(RuntimeError::InvalidPhase);
        }
        if text.is_empty() {
            return Err(RuntimeError::EmptyCommandText);
        }

        let turn_id = self.next_turn_id;
        self.next_turn_id = turn_id
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let next_revision = self
            .canonical_revision
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;

        let records = vec![
            SessionPayload::TurnStarted { turn_id },
            SessionPayload::InputMessage { turn_id, text },
        ];

        self.phase = ConversationPhase::PersistingInput { turn_id };
        let receipt = match self
            .writer
            .as_mut()
            .ok_or(RuntimeError::InvalidLifecycle)?
            .append(&records, Durability::Durable)
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                self.phase = ConversationPhase::Idle;
                return Err(error.into());
            }
        };

        self.canonical_history.extend(records);
        self.canonical_revision = next_revision;
        self.phase = ConversationPhase::PreparingAttempt { turn_id };
        Ok(receipt)
    }


    /// Persists attempt identity before building and scheduling its model request.
    pub async fn start_model_request(&mut self) -> Result<RuntimeEffect, RuntimeError> {
        let turn_id = match &self.phase {
            ConversationPhase::PreparingAttempt { turn_id } => *turn_id,
            _ => return Err(RuntimeError::InvalidPhase),
        };

        let attempt_id = self.next_attempt_id;
        self.next_attempt_id = attempt_id
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let next_revision = self
            .canonical_revision
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let records = vec![SessionPayload::ModelAttemptStarted {
            turn_id,
            attempt_id,
        }];
        let mut request_history = Vec::with_capacity(self.canonical_history.len() + records.len());
        request_history.extend_from_slice(&self.canonical_history);
        request_history.extend_from_slice(&records);

        let request = self.configuration.ports.request_builder.build(
            next_revision,
            &self.configuration.model,
            &request_history,
            &self.queued_steering,
        )?;
        if request.provider_generation != self.configuration.ports.model_route.generation {
            return Err(RuntimeError::ProviderGenerationMismatch);
        }
        if request.history_revision != next_revision {
            return Err(RuntimeError::RequestRevisionMismatch);
        }

        match self
            .writer
            .as_mut()
            .ok_or(RuntimeError::InvalidLifecycle)?
            .append(&records, Durability::Durable)
            .await
        {
            Ok(_) => {}
            Err(error) => {
                self.phase = ConversationPhase::Failed {
                    turn_id: Some(turn_id),
                    message: error.to_string(),
                };
                return Err(error.into());
            }
        }

        self.canonical_history.extend(records);
        self.canonical_revision = next_revision;
        self.pending_tool_inputs.clear();
        self.pending_tool_calls.clear();
        self.take_queued_steering();
        let attempt_id = harness_model_api::ModelAttemptId(attempt_id);
        self.phase = ConversationPhase::AwaitingModel {
            turn_id,
            attempt_id: attempt_id.0,
        };
        Ok(RuntimeEffect::StartModel {
            attempt: Arc::new(ModelAttempt::initial(request, attempt_id)),
            route: self.configuration.ports.model_route.clone(),
        })
    }


    /// Marks an attempt as receiving model events.
    pub fn begin_streaming(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
    ) -> Result<(), RuntimeError> {
        if !matches!(
            &self.phase,
            ConversationPhase::AwaitingModel {
                turn_id: active_turn,
                attempt_id: active_attempt,
            } if *active_turn == turn_id && *active_attempt == attempt_id
        ) {
            return Err(RuntimeError::InvalidPhase);
        }

        self.phase = ConversationPhase::Streaming {
            turn_id,
            attempt_id,
        };
        Ok(())
    }

    /// Records transient assistant text without changing canonical history.
    pub fn record_assistant_delta(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
        delta: String,
    ) -> Result<(), RuntimeError> {
        if !matches!(
            &self.phase,
            ConversationPhase::Streaming {
                turn_id: active_turn,
                attempt_id: active_attempt,
            } if *active_turn == turn_id && *active_attempt == attempt_id
        ) {
            return Err(RuntimeError::InvalidPhase);
        }

        self.transient_assistant.push_str(&delta);
        Ok(())
    }

    /// Applies one typed model event to the active attempt.
    pub async fn dispatch_model_event(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
        event: ModelEvent,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        match event {
            ModelEvent::Started
                if matches!(
                    &self.phase,
                    ConversationPhase::Streaming {
                        turn_id: active_turn,
                        attempt_id: active_attempt,
                    } if *active_turn == turn_id && *active_attempt == attempt_id
                ) =>
            {
                Err(RuntimeError::DuplicateModelStart)
            }
            ModelEvent::Started => {
                self.begin_streaming(turn_id, attempt_id)?;
                Ok(vec![RuntimeEffect::Emit(RuntimeEvent::ResponseStarted)])
            }
            ModelEvent::AssistantTextDelta(delta) => {
                self.record_assistant_delta(turn_id, attempt_id, delta.clone())?;
                Ok(vec![RuntimeEffect::Emit(RuntimeEvent::AssistantTextDelta(
                    delta,
                ))])
            }
            ModelEvent::ToolInputDelta(delta) => {
                self.ensure_streaming_attempt(turn_id, attempt_id)?;
                self.pending_tool_inputs
                    .entry(delta.call_id)
                    .or_default()
                    .push_str(&delta.fragment);
                Ok(Vec::new())
            }
            ModelEvent::ToolCall(call) => {
                self.ensure_streaming_attempt(turn_id, attempt_id)?;
                if self
                    .pending_tool_calls
                    .iter()
                    .any(|pending| pending.call_id == call.call_id)
                {
                    return Err(RuntimeError::DuplicateToolCall);
                }
                self.pending_tool_inputs.remove(&call.call_id);
                self.pending_tool_calls.push(call);
                Ok(Vec::new())
            }
            ModelEvent::Metadata(_) | ModelEvent::Usage(_) => {
                self.ensure_streaming_attempt(turn_id, attempt_id)?;
                Ok(Vec::new())
            }
            ModelEvent::Terminal(_) if !self.pending_tool_inputs.is_empty() => {
                Err(RuntimeError::IncompleteToolInput)
            }
            ModelEvent::Terminal(outcome) => {
                self.finish_model_attempt(turn_id, attempt_id, outcome.clone())
                    .await?;
                Ok(vec![RuntimeEffect::Emit(RuntimeEvent::ResponseFinished(
                    outcome,
                ))])
            }
        }
    }

    fn ensure_streaming_attempt(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
    ) -> Result<(), RuntimeError> {
        match &self.phase {
            ConversationPhase::AwaitingModel {
                turn_id: active_turn,
                attempt_id: active_attempt,
            } if *active_turn == turn_id && *active_attempt == attempt_id => {
                self.phase = ConversationPhase::Streaming {
                    turn_id,
                    attempt_id,
                };
                Ok(())
            }
            ConversationPhase::Streaming {
                turn_id: active_turn,
                attempt_id: active_attempt,
            } if *active_turn == turn_id && *active_attempt == attempt_id => Ok(()),
            _ => Err(RuntimeError::InvalidPhase),
        }
    }

    /// Persists the accumulated assistant text before allowing continuation.
    pub async fn commit_assistant(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
    ) -> Result<AppendReceipt, RuntimeError> {
        if self.phase
            != (ConversationPhase::PersistingAssistant {
                turn_id,
                attempt_id,
            })
        {
            return Err(RuntimeError::InvalidPhase);
        }

        let next_revision = self
            .canonical_revision
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let records = vec![SessionPayload::AssistantMessage {
            turn_id,
            text: self.transient_assistant.clone(),
        }];
        self.phase = ConversationPhase::PersistingAssistant {
            turn_id,
            attempt_id,
        };
        let receipt = match self
            .writer
            .as_mut()
            .ok_or(RuntimeError::InvalidLifecycle)?
            .append(&records, Durability::Durable)
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                self.phase = ConversationPhase::Failed {
                    turn_id: Some(turn_id),
                    message: error.to_string(),
                };
                return Err(error.into());
            }
        };

        self.canonical_history.extend(records);
        self.canonical_revision = next_revision;
        self.transient_assistant.clear();
        self.pending_interrupt = None;
        self.phase = ConversationPhase::PreparingContinuation { turn_id };
        Ok(receipt)
    }

    /// Persists turn completion after assistant and tool work finish.
    pub async fn complete_turn(
        &mut self,
        turn_id: u64,
    ) -> Result<AppendReceipt, RuntimeError> {
        if self.phase != (ConversationPhase::PreparingContinuation { turn_id }) {
            return Err(RuntimeError::InvalidPhase);
        }
        if !self.pending_tool_calls.is_empty() {
            return Err(RuntimeError::PendingToolCalls);
        }
        let next_revision = self
            .canonical_revision
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let records = vec![SessionPayload::TurnFinished {
            turn_id,
            outcome: TurnOutcome::Completed,
        }];
        match self
            .writer
            .as_mut()
            .ok_or(RuntimeError::InvalidLifecycle)?
            .append(&records, Durability::Durable)
            .await
        {
            Ok(receipt) => {
                self.canonical_history.extend(records);
                self.canonical_revision = next_revision;
                self.phase = ConversationPhase::Idle;
                Ok(receipt)
            }
            Err(error) => {
                self.phase = ConversationPhase::Failed {
                    turn_id: Some(turn_id),
                    message: error.to_string(),
                };
                Err(error.into())
            }
        }
    }

    /// Requests cancellation for the active model attempt.
    pub fn interrupt(&mut self, turn_id: u64, attempt_id: u64) -> Result<(), RuntimeError> {
        match &self.phase {
            ConversationPhase::AwaitingModel {
                turn_id: active_turn,
                attempt_id: active_attempt,
            }
            | ConversationPhase::Streaming {
                turn_id: active_turn,
                attempt_id: active_attempt,
            } if *active_turn == turn_id && *active_attempt == attempt_id => {
                self.phase = ConversationPhase::Cancelling {
                    turn_id,
                    attempt_id,
                };
                Ok(())
            }
            _ => Err(RuntimeError::InvalidPhase),
        }
    }

    /// Persists a terminal non-completion outcome before returning to idle.
    pub async fn finish_model_attempt(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
        outcome: ModelTerminalOutcome,
    ) -> Result<(), RuntimeError> {
        let active = matches!(
            &self.phase,
            ConversationPhase::AwaitingModel {
                turn_id: active_turn,
                attempt_id: active_attempt,
            }
                | ConversationPhase::Streaming {
                    turn_id: active_turn,
                    attempt_id: active_attempt,
                }
                | ConversationPhase::Cancelling {
                    turn_id: active_turn,
                    attempt_id: active_attempt,
                } if *active_turn == turn_id && *active_attempt == attempt_id
        );
        if !active {
            return Err(RuntimeError::InvalidPhase);
        }

        if matches!(outcome, ModelTerminalOutcome::Completed(_)) {
            self.phase = ConversationPhase::PersistingAssistant {
                turn_id,
                attempt_id,
            };
            return Ok(());
        }

        let next_revision = self
            .canonical_revision
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let persisted_outcome = match &outcome {
            ModelTerminalOutcome::Interrupted(interruption) => TurnOutcome::Interrupted {
                reason: interruption.reason.clone(),
            },
            ModelTerminalOutcome::Cancelled(cancellation) => TurnOutcome::Cancelled {
                reason: cancellation.reason.clone(),
            },
            ModelTerminalOutcome::Failed(failure) => TurnOutcome::Failed {
                message: failure.message.clone(),
            },
            ModelTerminalOutcome::Completed(_) => unreachable!(),
        };
        let records = vec![SessionPayload::TurnFinished {
            turn_id,
            outcome: persisted_outcome,
        }];
        self.phase = ConversationPhase::Cancelling {
            turn_id,
            attempt_id,
        };

        match self
            .writer
            .as_mut()
            .ok_or(RuntimeError::InvalidLifecycle)?
            .append(&records, Durability::Durable)
            .await
        {
            Ok(_) => {}
            Err(error) => {
                self.phase = ConversationPhase::Failed {
                    turn_id: Some(turn_id),
                    message: error.to_string(),
                };
                return Err(error.into());
            }
        }

        self.canonical_history.extend(records);
        self.canonical_revision = next_revision;
        self.transient_assistant.clear();
        self.pending_interrupt = None;
        self.phase = ConversationPhase::Idle;
        Ok(())
    }

    /// Returns the explicit persist-mode state.
    pub fn persist_state(&self) -> &PersistState {
        &self.persist_state
    }

    /// Builds an active attempt only when its request and route share a provider generation.
    pub fn build_active_attempt(
        turn_id: u64,
        attempt: Arc<ModelAttempt>,
        route: ResolvedModelRoute,
    ) -> Result<ActiveModelAttempt, RuntimeError> {
        if attempt.request.provider_generation != route.generation {
            return Err(RuntimeError::ProviderGenerationMismatch);
        }
        Ok(ActiveModelAttempt {
            turn_id,
            attempt,
            route,
            phase: AttemptPhase::Created,
            assistant_text: String::new(),
            terminal_outcome: None,
        })
    }

    /// Applies one frontend command and returns typed effects.
    pub async fn dispatch_command(
        &mut self,
        command: RuntimeCommand,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        match command {
            RuntimeCommand::SubmitPrompt { text } => {
                let persisted_text = text.clone();
                let receipt = self.submit_prompt(text).await?;
                Ok(vec![RuntimeEffect::Emit(RuntimeEvent::TranscriptAppended(
                    harness_runtime_api::TranscriptSnapshotEntry {
                        sequence: receipt.sequences.clone().last(),
                        payload: harness_runtime_api::TranscriptPayload::PlainText(persisted_text),
                    },
                ))])
            }
            RuntimeCommand::QueueSteering { text } => {
                if self.lifecycle != RuntimeLifecycle::Ready {
                    return Err(RuntimeError::InvalidLifecycle);
                }
                if text.is_empty() {
                    return Err(RuntimeError::EmptyCommandText);
                }
                if !matches!(
                    &self.phase,
                    ConversationPhase::AwaitingModel { .. }
                        | ConversationPhase::Streaming { .. }
                        | ConversationPhase::PreparingContinuation { .. }
                ) {
                    return Err(RuntimeError::InvalidPhase);
                }
                self.queued_steering.push(text);
                Ok(Vec::new())
            }
            RuntimeCommand::Interrupt { text } => {
                if text.is_empty() {
                    return Err(RuntimeError::EmptyCommandText);
                }
                let (turn_id, attempt_id) = match &self.phase {
                    ConversationPhase::AwaitingModel {
                        turn_id,
                        attempt_id,
                    }
                    | ConversationPhase::Streaming {
                        turn_id,
                        attempt_id,
                    } => (*turn_id, *attempt_id),
                    _ => return Err(RuntimeError::InvalidPhase),
                };
                self.pending_interrupt = Some(text);
                self.interrupt(turn_id, attempt_id)?;
                Ok(Vec::new())
            }
            RuntimeCommand::SetModel { selection } => {
                if self.lifecycle != RuntimeLifecycle::Ready {
                    return Err(RuntimeError::InvalidLifecycle);
                }
                if self.phase != ConversationPhase::Idle {
                    return Err(RuntimeError::InvalidPhase);
                }
                if self.configuration.ports.model_route.selection != selection {
                    return Err(RuntimeError::ModelRouteSelectionMismatch);
                }
                self.configuration.model = selection;
                Ok(Vec::new())
            }
            RuntimeCommand::LoadOlderTranscript { before_sequence } => {
                if self.lifecycle != RuntimeLifecycle::Ready {
                    return Err(RuntimeError::InvalidLifecycle);
                }
                Ok(vec![RuntimeEffect::LoadTranscriptPage {
                    session_id: self.configuration.session_id.clone(),
                    before_sequence,
                    page_size: PageSize::DEFAULT,
                }])
            }
            RuntimeCommand::Shutdown => {
                self.begin_shutdown()?;
                Ok(Vec::new())
            }

        }
    }


    /// Persists the next queued tool call before allowing its executor to run.
    pub async fn accept_next_tool_call(
        &mut self,
        turn_id: u64,
    ) -> Result<AppendReceipt, RuntimeError> {
        if self.phase != (ConversationPhase::PreparingContinuation { turn_id }) {
            return Err(RuntimeError::InvalidPhase);
        }
        let call = self
            .pending_tool_calls
            .first()
            .ok_or(RuntimeError::NoPendingToolCall)?
            .clone();
        let tool = ToolName::new(call.name.clone()).map_err(|_| RuntimeError::UnknownTool)?;
        if self.configuration.ports.tool_registry.get(&tool).is_none() {
            return Err(RuntimeError::UnknownTool);
        }
        let call_id = call.call_id.clone();

        let execution_id = self.next_execution_id;
        self.next_execution_id = execution_id
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let next_revision = self
            .canonical_revision
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;

        let records = vec![
            SessionPayload::ToolCallAccepted {
                turn_id,
                call_id: call_id.clone(),
                name: call.name.clone(),
                input: call.input.as_str().to_owned(),
            },
            SessionPayload::ToolExecutionStarted {
                turn_id,
                call_id: call_id.clone(),
            },
        ];
        self.phase = ConversationPhase::PersistingToolCall {
            turn_id,
            call_id: call_id.clone(),
        };
        let receipt = match self
            .writer
            .as_mut()
            .ok_or(RuntimeError::InvalidLifecycle)?
            .append(&records, Durability::Durable)
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                self.phase = ConversationPhase::Failed {
                    turn_id: Some(turn_id),
                    message: error.to_string(),
                };
                return Err(error.into());
            }
        };
        self.canonical_history.extend(records);
        self.canonical_revision = next_revision;
        self.pending_tool_calls.remove(0);
        self.active_tool_call = Some(call);
        self.phase = ConversationPhase::ExecutingTool {
            turn_id,
            execution_id,
            call_id,
        };
        Ok(receipt)
    }

    /// Builds an execution request for the durably accepted active tool call.
    pub fn active_tool_execution_request(
        &self,
        policy: ToolExecutionPolicy,
    ) -> Result<ToolExecutionRequest, RuntimeError> {
        let execution_id = match &self.phase {
            ConversationPhase::ExecutingTool { execution_id, .. } => *execution_id,
            _ => return Err(RuntimeError::InvalidPhase),
        };
        let call = self
            .active_tool_call
            .as_ref()
            .ok_or(RuntimeError::InvalidPhase)?;
        let tool = ToolName::new(call.name.clone()).map_err(|_| RuntimeError::UnknownTool)?;
        if self.configuration.ports.tool_registry.get(&tool).is_none() {
            return Err(RuntimeError::UnknownTool);
        }
        Ok(ToolExecutionRequest {
            execution_id: ToolExecutionId(execution_id),
            tool,
            input: call.input.clone(),
            policy,
        })
    }

    /// Persists a tool result before allowing a continuation request.
    pub async fn commit_tool_result(
        &mut self,
        turn_id: u64,
        execution_id: u64,
        call_id: String,
        output: String,
    ) -> Result<AppendReceipt, RuntimeError> {
        if self.phase
            != (ConversationPhase::ExecutingTool {
                turn_id,
                execution_id,
                call_id: call_id.clone(),
            })
        {
            return Err(RuntimeError::InvalidPhase);
        }

        let next_revision = self
            .canonical_revision
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let records = vec![SessionPayload::ToolExecutionFinished {
            turn_id,
            call_id: call_id.clone(),
            output,
        }];
        self.phase = ConversationPhase::PersistingToolResult {
            turn_id,
            execution_id,
            call_id,
        };
        let receipt = match self
            .writer
            .as_mut()
            .ok_or(RuntimeError::InvalidLifecycle)?
            .append(&records, Durability::Durable)
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                self.phase = ConversationPhase::Failed {
                    turn_id: Some(turn_id),
                    message: error.to_string(),
                };
                return Err(error.into());
            }
        };
        self.canonical_history.extend(records);
        self.canonical_revision = next_revision;
        self.active_tool_call = None;
        self.phase = ConversationPhase::PreparingContinuation { turn_id };
        Ok(receipt)
    }

    /// Projects one persisted page into exactly representable frontend entries.
    pub fn apply_transcript_page(&self, page: StoredTranscriptPage) -> RuntimeEffect {
        let entries = page
            .entries
            .into_iter()
            .filter_map(|record| {
                let payload = match record.payload {
                    SessionPayload::InputMessage { text, .. } => TranscriptPayload::Message {
                        role: MessageRole::User,
                        text,
                    },
                    SessionPayload::AssistantMessage { text, .. } => TranscriptPayload::Message {
                        role: MessageRole::Assistant,
                        text,
                    },
                    SessionPayload::ToolExecutionFinished {
                        call_id, output, ..
                    } => TranscriptPayload::ToolResult { call_id, output },
                    SessionPayload::Metadata(_)
                    | SessionPayload::ProviderBinding(_)
                    | SessionPayload::TurnStarted { .. }
                    | SessionPayload::ModelAttemptStarted { .. }
                    | SessionPayload::ToolCallAccepted { .. }
                    | SessionPayload::ToolExecutionStarted { .. }
                    | SessionPayload::TurnFinished { .. }
                    | SessionPayload::SessionClosed => return None,
                };
                Some(TranscriptSnapshotEntry {
                    sequence: Some(record.sequence),
                    payload,
                })
            })
            .collect();

        RuntimeEffect::Emit(RuntimeEvent::TranscriptPageLoaded(
            harness_runtime_api::TranscriptPage {
                entries,
                next_before_sequence: page.next_before,
                reached_start: page.reached_start,
            },
        ))
    }

    pub fn canonical_history(&self) -> &[SessionPayload] {
        &self.canonical_history
    }

    /// Creates the shutdown acknowledgment only after joined shutdown completes.
    pub fn shutdown_complete_event(&self) -> Result<RuntimeEvent, RuntimeError> {
        if self.lifecycle != RuntimeLifecycle::Stopped {
            return Err(RuntimeError::InvalidLifecycle);
        }
        Ok(RuntimeEvent::ShutdownComplete)
    }

    /// Returns the transient assistant text that is not yet canonical.
    pub fn transient_assistant(&self) -> &str {
        &self.transient_assistant
    }

    /// Returns the current lifecycle.
    pub fn lifecycle(&self) -> RuntimeLifecycle {
        self.lifecycle
    }

    /// Returns queued steering text in insertion order.
    pub fn queued_steering(&self) -> &[String] {
        &self.queued_steering
    }

    /// Takes the queued steering text for one immutable model attempt.
    pub fn take_queued_steering(&mut self) -> Vec<String> {
        std::mem::take(&mut self.queued_steering)
    }

    /// Takes the exact interrupt text associated with the active attempt.
    pub fn take_pending_interrupt(&mut self) -> Option<String> {
        self.pending_interrupt.take()
    }

    /// Returns partial tool inputs retained for active model calls.
    pub fn pending_tool_inputs(&self) -> &BTreeMap<String, String> {
        &self.pending_tool_inputs
    }


    /// Returns the canonical history revision used by compaction and model requests.
    pub fn canonical_revision(&self) -> u64 {
        self.canonical_revision
    }

    /// Returns the current mutually exclusive conversation phase.
    pub fn phase(&self) -> &ConversationPhase {
        &self.phase
    }

    /// Returns the configured session identity.
    pub fn session_id(&self) -> harness_session_store::SessionId {
        self.configuration.session_id.clone()
    }
}

/// Immutable compaction plan bound to one canonical history revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    /// Canonical revision used to compute the plan.
    pub base_revision: u64,
    /// Inclusive record range replaced by compaction.
    pub source_range: std::ops::RangeInclusive<u64>,
    /// Replacement summary text.
    pub replacement: String,
}

/// Creates a compaction plan without mutating runtime history.
pub fn plan_compaction(
    base_revision: u64,
    source_range: std::ops::RangeInclusive<u64>,
    replacement: String,
) -> CompactionPlan {
    CompactionPlan {
        base_revision,
        source_range,
        replacement,
    }
}

/// Commits a plan only when the canonical revision is unchanged.
pub fn commit_compaction(
    current_revision: &mut u64,
    plan: CompactionPlan,
) -> Result<(), RuntimeError> {
    if *current_revision != plan.base_revision {
        return Err(RuntimeError::StaleCompaction);
    }
    *current_revision = current_revision
        .checked_add(1)
        .ok_or(RuntimeError::IdExhausted)?;
    Ok(())
}

/// Failure returned by runtime construction or lifecycle transitions.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Session storage fails.
    #[error(transparent)]
    Session(#[from] SessionStoreError),
    /// Model transport shutdown fails.
    #[error("model transport shutdown failed: {0:?}")]
    ModelTransport(harness_model_api::ModelFailure),
    /// Runtime receives a completion for an unknown job.
    #[error("runtime received an unknown job completion")]
    UnknownJob,
    /// Runtime receives a duplicate completion.
    #[error("runtime received a duplicate job completion")]
    DuplicateJob,
    /// A supervised job cannot be joined.
    #[error("runtime job failed to join: {0}")]
    JobJoin(String),
    /// Runtime lifecycle does not permit the requested operation.
    #[error("runtime lifecycle does not permit this operation")]
    InvalidLifecycle,
    /// Runtime phase does not permit the requested operation.
    #[error("runtime phase does not permit this operation")]
    InvalidPhase,
    /// Identifier allocation is exhausted.
    #[error("runtime identifier allocation is exhausted")]
    IdExhausted,
    /// A compaction result was built from an obsolete canonical revision.
    #[error("compaction result is stale")]
    StaleCompaction,
    /// A model attempt and route belong to different provider generations.
    #[error("model attempt and provider route use different generations")]
    ProviderGenerationMismatch,
    /// A model request does not represent the current canonical revision.
    #[error("model request does not match the canonical history revision")]
    RequestRevisionMismatch,
    /// A terminal event arrives while tool input remains incomplete.
    #[error("model stream ended with incomplete tool input")]
    IncompleteToolInput,
    /// A model attempt emits the same completed tool call more than once.
    #[error("model attempt emitted a duplicate completed tool call")]
    DuplicateToolCall,
    /// A continuation cannot return idle while completed tool calls remain.
    #[error("completed tool calls remain pending")]
    PendingToolCalls,
    /// No completed tool call is available for durable acceptance.
    #[error("no completed tool call is pending")]
    NoPendingToolCall,
    /// A model requests a tool that is not registered.
    #[error("model requested an unknown tool")]
    UnknownTool,
    /// A selected model does not match the configured provider route.
    #[error("selected model does not match the configured provider route")]
    ModelRouteSelectionMismatch,
    /// A prompt, steering command, or interrupt contains no text.
    #[error("runtime command text is empty")]
    EmptyCommandText,
    /// A model attempt emits more than one start event.
    #[error("model attempt emitted a duplicate start event")]
    DuplicateModelStart,
}

/// Port that builds immutable model requests from canonical state.
pub trait ModelRequestBuilder: Send + Sync {
    /// Builds a request for the current canonical revision and selected model.
    fn build(
        &self,
        revision: u64,
        selection: &ModelSelection,
        history: &[SessionPayload],
        steering: &[String],
    ) -> Result<Arc<ModelRequest>, RuntimeError>;
}

/// Effect scheduled by a reducer transition.
#[derive(Debug)]
pub enum RuntimeEffect {
    /// Append records with explicit durability.
    Append {
        /// Payloads to append.
        records: Vec<SessionPayload>,
        /// Required durability.
        durability: Durability,
    },
    /// Start one model attempt.
    StartModel {
        /// Attempt to submit.
        attempt: Arc<ModelAttempt>,
        /// Route selected for this attempt.
        route: ResolvedModelRoute,
    },
    /// Loads one older transcript page outside the reducer.
    LoadTranscriptPage {
        /// Session to query.
        session_id: harness_session_store::SessionId,
        /// Exclusive sequence cursor.
        before_sequence: Option<u64>,
        /// Bounded page size.
        page_size: PageSize,
    },
    /// Emit one frontend event.
    Emit(RuntimeEvent),
    /// Await a frontend command.
    ContinueReceiving(RuntimeCommand),
}

/// Durable append completion returned to the reducer.
#[derive(Debug)]
pub struct AppendCompletion {
    /// Receipt proving the achieved durability.
    pub receipt: AppendReceipt,
    /// Records acknowledged by storage.
    pub records: Vec<SessionPayload>,
}


/// Writer capability used by the runtime after it has acquired a unique lease.
pub struct RuntimeSessionWriter {
    writer: Box<dyn SessionWriter>,
}

impl RuntimeSessionWriter {
    /// Creates a runtime writer from an acquired session lease.
    pub fn new(writer: Box<dyn SessionWriter>) -> Self {
        Self { writer }
    }

    /// Appends records without changing in-memory canonical state first.
    pub async fn append(
        &mut self,
        records: &[SessionPayload],
        durability: Durability,
    ) -> Result<AppendReceipt, SessionStoreError> {
        self.writer.append(records, durability).await
    }

    /// Closes the writer and joins its owned work.
    pub async fn close(self) -> Result<(), SessionStoreError> {
        self.writer.close().await
    }
}
