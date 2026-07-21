//! Conversation policy and lifecycle contracts above injected ports.
//!
//! This crate does not parse provider wire data, open files, resolve
//! credentials, or implement PTYs. The session store remains behind its
//! current-format adapter until the final user-run migration phase.

use std::{collections::BTreeMap, sync::{Arc, RwLock}};

pub mod compaction;

use harness_model_api::{
    ModelAttempt, ModelEvent, ModelRequest, ModelSelection, ModelTerminalOutcome, ModelTransport,
    ProviderGeneration, ResolvedModelRoute,
};
use harness_runtime_api::{
    MessageRole, RuntimeCommand, RuntimeEvent, RuntimeFailureCategory, TranscriptPayload,
    TranscriptSnapshotEntry,
};
use harness_session_store::{
    AppendReceipt, Durability, PageSize, SessionErrorCategory, SessionPayload, SessionStore,
    SessionStoreError, SessionWriter, TranscriptPage as StoredTranscriptPage, TurnOutcome,
};
use harness_tool_api::{
    ToolAvailability, ToolExecutionId, ToolExecutionPolicy, ToolExecutionRequest, ToolExecutor,
    ToolName, ToolRegistry,
};
use thiserror::Error;
use tokio::task::JoinHandle;
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
    /// The active model attempt accepts events from the composition root.
    AwaitingModel { turn_id: u64, attempt_id: u64 },

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
    /// A compaction model attempt is active.
    Compacting { compaction_id: u64, attempt_id: u64 },
    /// A turn or lifecycle operation failed and is returning to idle.
    Failed {
        turn_id: Option<u64>,
        message: String,
    },
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
            ModelEvent::ReasoningSummaryDelta(_)
            | ModelEvent::ReasoningContentDelta(_)
            | ModelEvent::ReasoningItem(_) => {
                self.phase = AttemptPhase::Streaming;
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
            if let Err(error) = job.handle.await
                && first_error.is_none()
            {
                first_error = Some(RuntimeError::JobJoin(error.to_string()));
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
    pub tool_executor: Option<Arc<dyn ToolExecutor>>,
    /// Model transport implementation.
    pub model_transport: Arc<dyn ModelTransport>,
    /// Builds immutable semantic model requests.
    pub request_builder: Arc<dyn ModelRequestBuilder>,
    /// Route selected for root model requests.
    pub model_route: ResolvedModelRoute,
    /// Route selected for compaction requests.
    pub compaction_route: ResolvedModelRoute,
    /// Shared dynamic tool-availability policy.
    pub tool_availability: Arc<RwLock<ToolAvailability>>,
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
    transient_reasoning_content: String,
    transient_reasoning_summary: String,
    transient_reasoning_encrypted: Option<String>,
    pending_tool_inputs: BTreeMap<String, String>,
    pending_tool_calls: Vec<harness_model_api::ToolCall>,
    active_tool_call: Option<harness_model_api::ToolCall>,
    next_execution_id: u64,
    persist_state: PersistState,
    queued_steering: Vec<String>,
    pending_interrupt: Option<String>,
    request_loop_stopped: bool,
    canonical_revision: u64,
    transport_stopped: bool,
    next_attempt_id: u64,
    model_started: bool,
    compaction: Option<compaction::CompactionCoordinator>,
    next_compaction_id: u64,
    compatibility_mode: bool,

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
            transient_reasoning_content: String::new(),
            transient_reasoning_summary: String::new(),
            transient_reasoning_encrypted: None,
            pending_tool_inputs: BTreeMap::new(),
            pending_tool_calls: Vec::new(),
            active_tool_call: None,
            next_turn_id: 1,
            next_execution_id: 1,
            persist_state: PersistState::Disabled,
            queued_steering: Vec::new(),
            pending_interrupt: None,
            request_loop_stopped: false,
            canonical_revision: 0,
            transport_stopped: false,
            next_attempt_id: 1,
            model_started: false,
            compaction: None,
            next_compaction_id: 1,
            compatibility_mode: false,
        }
    }

    /// Acquires the unique session writer and enters startup.
    pub async fn begin_startup(&mut self) -> Result<(), RuntimeError> {
        if self.lifecycle != RuntimeLifecycle::Constructed {
            return Err(RuntimeError::InvalidLifecycle);
        }
        let history = self.load_persisted_history()?;
        self.restore_history(history)?;
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

    fn load_persisted_history(&self) -> Result<Vec<SessionPayload>, RuntimeError> {
        let reader = self.configuration.ports.session_store.reader()?;
        let mut before_sequence = None;
        let mut records = Vec::new();

        loop {
            let page = match reader.load_older(
                self.configuration.session_id.clone(),
                before_sequence,
                PageSize::DEFAULT,
            ) {
                Ok(page) => page,
                Err(SessionStoreError::NotFound(_)) if before_sequence.is_none() => {
                    return Ok(records);
                }
                Err(error) => return Err(error.into()),
            };
            let next_before_sequence = page.next_before;
            records.extend(page.entries.into_iter().map(|record| record.payload));
            if page.reached_start {
                break;
            }
            let Some(next_before_sequence) = next_before_sequence else {
                return Err(SessionStoreError::InvalidFormat(
                    "nonterminal transcript page has no continuation cursor".to_string(),
                )
                .into());
            };
            if before_sequence.is_some_and(|before| next_before_sequence >= before) {
                return Err(SessionStoreError::InvalidFormat(
                    "transcript page cursor does not move toward the session start".to_string(),
                )
                .into());
            }
            before_sequence = Some(next_before_sequence);
        }

        records.reverse();
        Ok(records)
    }

    fn restore_history(&mut self, history: Vec<SessionPayload>) -> Result<(), RuntimeError> {
        let revision = u64::try_from(history.len()).map_err(|_| RuntimeError::IdExhausted)?;
        let mut highest_turn_id = 0;
        let mut highest_attempt_id = 0;
        for payload in &history {
            match payload {
                SessionPayload::TurnStarted { turn_id }
                | SessionPayload::InputMessage { turn_id, .. }
                | SessionPayload::ModelAttemptStarted { turn_id, .. }
                | SessionPayload::AssistantMessage { turn_id, .. }
                | SessionPayload::Reasoning { turn_id, .. }
                | SessionPayload::Error { turn_id, .. }
                | SessionPayload::ToolCallAccepted { turn_id, .. }
                | SessionPayload::ToolExecutionStarted { turn_id, .. }
                | SessionPayload::ToolExecutionFinished { turn_id, .. }
                | SessionPayload::TurnFinished { turn_id, .. } => {
                    highest_turn_id = highest_turn_id.max(*turn_id);
                }
                SessionPayload::Metadata(_)
                | SessionPayload::ProviderBinding(_)
                | SessionPayload::CompactionCheckpoint { .. }
                | SessionPayload::SessionClosed => {}
            }
            if let SessionPayload::ModelAttemptStarted { attempt_id, .. } = payload {
                highest_attempt_id = highest_attempt_id.max(*attempt_id);
            }
        }

        self.next_turn_id = highest_turn_id
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        self.next_attempt_id = highest_attempt_id
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        self.canonical_revision = revision;
        self.canonical_history = history;
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

        self.request_loop_stopped = false;
        let turn_id = self.next_turn_id;
        self.next_turn_id = turn_id.checked_add(1).ok_or(RuntimeError::IdExhausted)?;
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
        self.next_attempt_id = attempt_id.checked_add(1).ok_or(RuntimeError::IdExhausted)?;
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

        let request = if self.compatibility_mode {
            self.configuration.ports.request_builder.build_compatibility(
                next_revision,
                &self.configuration.model,
                self.configuration.ports.model_route.generation,
                &request_history,
                &self.queued_steering,
            )?
        } else {
            self.configuration.ports.request_builder.build(
                next_revision,
                &self.configuration.model,
                self.configuration.ports.model_route.generation,
                &request_history,
                &self.queued_steering,
            )?
        };
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
        self.transient_reasoning_content.clear();
        self.transient_reasoning_summary.clear();
        self.transient_reasoning_encrypted = None;
        self.take_queued_steering();
        self.model_started = false;
        let attempt_id = harness_model_api::ModelAttemptId(attempt_id);
        self.phase = ConversationPhase::AwaitingModel {
            turn_id,
            attempt_id: attempt_id.0,
        };
        Ok(RuntimeEffect::StartModel {
            turn_id,
            attempt: Arc::new(ModelAttempt::initial(request, attempt_id)),
            route: self.configuration.ports.model_route.clone(),
        })
    }

    /// Verifies that model events belong to the active attempt.
    /// Accepts both `AwaitingModel` (streaming) and `Cancelling` (draining after
    /// interrupt) because the transport may deliver buffered events after the
    /// cancellation signal is sent but before the terminal event arrives.
    pub fn ensure_model_attempt(&self, turn_id: u64, attempt_id: u64) -> Result<(), RuntimeError> {
        match &self.phase {
            ConversationPhase::AwaitingModel {
                turn_id: active_turn,
                attempt_id: active_attempt,
            }
            | ConversationPhase::Cancelling {
                turn_id: active_turn,
                attempt_id: active_attempt,
            } if *active_turn == turn_id && *active_attempt == attempt_id => Ok(()),
            _ => Err(RuntimeError::InvalidPhase),
        }
    }

    /// Records transient assistant text without changing canonical history.
    pub fn record_assistant_delta(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
        delta: String,
    ) -> Result<(), RuntimeError> {
        self.ensure_model_attempt(turn_id, attempt_id)?;
        self.transient_assistant.push_str(&delta);
        Ok(())
    }

    /// Prepares and schedules a compaction model request.
    pub fn start_compaction_request(&mut self) -> Result<RuntimeEffect, RuntimeError> {
        let coordinator = self
            .compaction
            .as_ref()
            .ok_or(RuntimeError::CompactionNotActive)?;
        if !matches!(self.phase, ConversationPhase::Idle) {
            return Err(RuntimeError::InvalidPhase);
        }
        let source = coordinator
            .source()
            .ok_or(RuntimeError::CompactionRedoUnavailable)?;
        let instruction = coordinator
            .instruction()
            .ok_or(RuntimeError::CompactionNotActive)?;
        let compaction_id = self.next_compaction_id;
        self.next_compaction_id = compaction_id
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let attempt_id = self.next_attempt_id;
        self.next_attempt_id = attempt_id.checked_add(1).ok_or(RuntimeError::IdExhausted)?;
        let request = self.configuration.ports.request_builder.build_compaction(
            self.canonical_revision,
            &self.configuration.model,
            self.configuration.ports.compaction_route.generation,
            &source.history,
            instruction,
        )?;
        if request.provider_generation != self.configuration.ports.compaction_route.generation {
            return Err(RuntimeError::ProviderGenerationMismatch);
        }
        self.model_started = false;
        self.phase = ConversationPhase::Compacting {
            compaction_id,
            attempt_id,
        };
        Ok(RuntimeEffect::StartCompaction {
            compaction_id,
            attempt: Arc::new(ModelAttempt::initial(
                request,
                harness_model_api::ModelAttemptId(attempt_id),
            )),
            route: self.configuration.ports.compaction_route.clone(),
        })
    }

    /// Applies one typed model event to the active attempt.
    pub async fn dispatch_model_event(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
        event: ModelEvent,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        if let ConversationPhase::Compacting {
            compaction_id,
            attempt_id: active_attempt,
        } = self.phase
        {
            if active_attempt != attempt_id {
                return Err(RuntimeError::InvalidPhase);
            }
            return match event {
                ModelEvent::Started => {
                    if self.model_started {
                        return Err(RuntimeError::DuplicateModelStart);
                    }
                    self.model_started = true;
                    Ok(vec![RuntimeEffect::Emit(RuntimeEvent::ResponseStarted)])
                }
                ModelEvent::AssistantTextDelta(delta) => {
                    self.record_compaction_delta(&delta)?;
                    Ok(vec![RuntimeEffect::Emit(RuntimeEvent::AssistantTextDelta(
                        delta,
                    ))])
                }
                ModelEvent::Terminal(outcome) => {
                    let started = self.model_started;
                    self.model_started = false;
                    self.phase = ConversationPhase::Idle;
                    if !started {
                        return Ok(vec![
                            RuntimeEffect::Emit(RuntimeEvent::AgenticLoopCompleted),
                            RuntimeEffect::Emit(RuntimeEvent::Failure(
                                harness_runtime_api::RuntimeFailure {
                                    category:
                                        harness_runtime_api::RuntimeFailureCategory::Protocol,
                                    message: "compaction stream completed before it started"
                                        .to_string(),
                                },
                            )),
                        ]);
                    }
                    match outcome {
                        ModelTerminalOutcome::Completed(completion) => {
                            self.finish_compaction()?;
                            let summary = self
                                .compaction
                                .as_ref()
                                .and_then(compaction::CompactionCoordinator::validated_summary)
                                .ok_or(RuntimeError::CompactionInvalidResult)?
                                .to_owned();
                            Ok(vec![
                                RuntimeEffect::Emit(RuntimeEvent::ResponseFinished(
                                    ModelTerminalOutcome::Completed(completion),
                                )),
                                RuntimeEffect::CommitCompaction {
                                    compaction_id,
                                    summary,
                                },
                            ])
                        }
                        other => Ok(vec![
                            RuntimeEffect::Emit(RuntimeEvent::AgenticLoopCompleted),
                            RuntimeEffect::Emit(RuntimeEvent::ResponseFinished(other)),
                        ]),
                    }
                }
                ModelEvent::ReasoningSummaryDelta(_)
                | ModelEvent::ReasoningContentDelta(_)
                | ModelEvent::ReasoningItem(_)
                | ModelEvent::ToolInputDelta(_)
                | ModelEvent::ToolCall(_)
                | ModelEvent::Metadata(_)
                | ModelEvent::Usage(_) => Ok(Vec::new()),
            };
        }

        match event {
            ModelEvent::Started => {
                self.ensure_model_attempt(turn_id, attempt_id)?;
                if self.model_started {
                    return Err(RuntimeError::DuplicateModelStart);
                }
                self.model_started = true;
                Ok(vec![RuntimeEffect::Emit(RuntimeEvent::ResponseStarted)])
            }
            ModelEvent::AssistantTextDelta(delta) => {
                self.record_assistant_delta(turn_id, attempt_id, delta.clone())?;
                Ok(vec![RuntimeEffect::Emit(RuntimeEvent::AssistantTextDelta(
                    delta,
                ))])
            }
            ModelEvent::ReasoningSummaryDelta(delta) => {
                self.ensure_model_attempt(turn_id, attempt_id)?;
                self.transient_reasoning_summary.push_str(&delta);
                Ok(vec![RuntimeEffect::Emit(
                    RuntimeEvent::ReasoningSummaryDelta(delta),
                )])
            }
            ModelEvent::ReasoningContentDelta(delta) => {
                self.ensure_model_attempt(turn_id, attempt_id)?;
                self.transient_reasoning_content.push_str(&delta);
                Ok(vec![RuntimeEffect::Emit(
                    RuntimeEvent::ReasoningContentDelta(delta),
                )])
            }
            ModelEvent::ReasoningItem(item) => {
                self.ensure_model_attempt(turn_id, attempt_id)?;
                let mut effects = Vec::new();
                if let Some(content) = item.content {
                    if self.transient_reasoning_content.is_empty() {
                        self.transient_reasoning_content = content.clone();
                        effects.push(RuntimeEffect::Emit(RuntimeEvent::ReasoningContentDelta(
                            content,
                        )));
                    }
                }
                if let Some(summary) = item.summary {
                    if self.transient_reasoning_summary.is_empty() {
                        self.transient_reasoning_summary = summary.clone();
                        effects.push(RuntimeEffect::Emit(RuntimeEvent::ReasoningSummaryDelta(
                            summary,
                        )));
                    }
                }
                if item.encrypted_content.is_some() {
                    self.transient_reasoning_encrypted = item.encrypted_content;
                }
                Ok(effects)
            }
            ModelEvent::ToolInputDelta(delta) => {
                self.ensure_model_attempt(turn_id, attempt_id)?;
                self.pending_tool_inputs
                    .entry(delta.call_id)
                    .or_default()
                    .push_str(&delta.fragment);
                Ok(Vec::new())
            }
            ModelEvent::ToolCall(call) => {
                self.ensure_model_attempt(turn_id, attempt_id)?;
                if self
                    .pending_tool_calls
                    .iter()
                    .any(|pending| pending.call_id == call.call_id)
                {
                    return Err(RuntimeError::DuplicateToolCall);
                }
                self.pending_tool_inputs.remove(&call.call_id);
                let transcript = RuntimeEffect::Emit(RuntimeEvent::TranscriptAppended(
                    harness_runtime_api::TranscriptSnapshotEntry {
                        sequence: None,
                        payload: harness_runtime_api::TranscriptPayload::ToolCall {
                            call_id: call.call_id.clone(),
                            name: call.name.clone(),
                            input: call.input.clone(),
                        },
                    },
                ));
                self.pending_tool_calls.push(call);
                Ok(vec![transcript])
            }
            ModelEvent::Metadata(_) | ModelEvent::Usage(_) => {
                self.ensure_model_attempt(turn_id, attempt_id)?;
                Ok(Vec::new())
            }
            ModelEvent::Terminal(outcome)
                if !self.pending_tool_inputs.is_empty() =>
            {
                // The model stream ended with partial tool input deltas that
                // never completed into full ToolCall events (e.g. user
                // interrupt or cancellation mid-tool-call).  Discard the
                // partial state so finish_model_attempt can record the
                // outcome cleanly instead of erroring out.
                self.pending_tool_inputs.clear();
                self.pending_tool_calls.clear();
                self.finish_model_attempt(turn_id, attempt_id, outcome)
                    .await
            }
            ModelEvent::Terminal(outcome) => {
                self.finish_model_attempt(turn_id, attempt_id, outcome)
                    .await
            }
        }
    }

    /// Persists a validated compaction checkpoint before replacing active history.
    pub async fn commit_compaction(
        &mut self,
        _compaction_id: u64,
        summary: String,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        if self.phase != ConversationPhase::Idle {
            return Err(RuntimeError::InvalidPhase);
        }
        let coordinator = self
            .compaction
            .as_ref()
            .ok_or(RuntimeError::CompactionNotActive)?;
        let source = coordinator
            .source()
            .ok_or(RuntimeError::CompactionRedoUnavailable)?
            .clone();
        let source_revision = source.revision;
        let turn_id = source
            .history
            .iter()
            .rev()
            .find_map(|payload| match payload {
                SessionPayload::TurnStarted { turn_id }
                | SessionPayload::InputMessage { turn_id, .. }
                | SessionPayload::ModelAttemptStarted { turn_id, .. }
                | SessionPayload::AssistantMessage { turn_id, .. }
                | SessionPayload::Reasoning { turn_id, .. }
                | SessionPayload::Error { turn_id, .. }
                | SessionPayload::ToolCallAccepted { turn_id, .. }
                | SessionPayload::ToolExecutionStarted { turn_id, .. }
                | SessionPayload::ToolExecutionFinished { turn_id, .. }
                | SessionPayload::TurnFinished { turn_id, .. } => Some(*turn_id),
                _ => None,
            })
            .unwrap_or(0);
        let record = SessionPayload::CompactionCheckpoint {
            source_revision,
            summary: summary.clone(),
        };
        let next_revision = self
            .canonical_revision
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        self.writer
            .as_mut()
            .ok_or(RuntimeError::InvalidLifecycle)?
            .append(std::slice::from_ref(&record), Durability::Durable)
            .await
            .map_err(RuntimeError::Session)?;
        self.canonical_history.clear();
        self.canonical_history.push(record);
        self.canonical_revision = next_revision;
        self.compaction
            .as_mut()
            .ok_or(RuntimeError::CompactionNotActive)?
            .commit()
            .map_err(|_| RuntimeError::CompactionInvalidResult)?;
        self.compaction = None;
        self.request_loop_stopped = false;
        self.phase = ConversationPhase::PreparingAttempt { turn_id };
        Ok(vec![
            RuntimeEffect::Emit(RuntimeEvent::CompactionCompleted(summary)),
            RuntimeEffect::ContinueModel { turn_id },
        ])
    }

    /// Persists the accumulated assistant text before allowing continuation.
    pub async fn commit_assistant(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
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
        let has_reasoning = !self.transient_reasoning_content.is_empty()
            || !self.transient_reasoning_summary.is_empty()
            || self.transient_reasoning_encrypted.is_some();
        let mut records = Vec::with_capacity(2);
        if has_reasoning {
            records.push(SessionPayload::Reasoning {
                turn_id,
                content: (!self.transient_reasoning_content.is_empty())
                    .then(|| self.transient_reasoning_content.clone()),
                encrypted_content: self.transient_reasoning_encrypted.clone(),
                summary: (!self.transient_reasoning_summary.is_empty())
                    .then(|| self.transient_reasoning_summary.clone()),
            });
        }
        records.push(SessionPayload::AssistantMessage {
            turn_id,
            text: self.transient_assistant.clone(),
        });
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
        let reasoning_sequence = has_reasoning.then(|| *receipt.sequences.start());
        let assistant_sequence = *receipt.sequences.end();

        self.canonical_history.extend(records);
        self.canonical_revision = next_revision;
        self.transient_assistant.clear();
        self.transient_reasoning_content.clear();
        self.transient_reasoning_summary.clear();
        self.transient_reasoning_encrypted = None;
        self.pending_interrupt = None;
        self.model_started = false;
        self.phase = ConversationPhase::PreparingContinuation { turn_id };
        Ok(vec![
            RuntimeEffect::Emit(RuntimeEvent::TranscriptCommitted {
                reasoning_sequence,
                assistant_sequence,
            }),
            RuntimeEffect::ContinueTurn { turn_id },
        ])
    }

    /// Persists turn completion after assistant and tool work finish.
    pub async fn complete_turn(&mut self, turn_id: u64) -> Result<AppendReceipt, RuntimeError> {
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
    /// No-ops if already cancelling (buffered events still draining).
    pub fn interrupt(&mut self, turn_id: u64, attempt_id: u64) -> Result<(), RuntimeError> {
        match &self.phase {
            ConversationPhase::AwaitingModel {
                turn_id: active_turn,
                attempt_id: active_attempt,
            } if *active_turn == turn_id && *active_attempt == attempt_id => {
                self.phase = ConversationPhase::Cancelling {
                    turn_id,
                    attempt_id,
                };
                Ok(())
            }
            // Already cancelling — the first interrupt already fired, transport
            // is draining, and we're just waiting for the terminal event.
            ConversationPhase::Cancelling { .. } => Ok(()),
            _ => Err(RuntimeError::InvalidPhase),
        }
    }

    /// Persists a terminal non-completion outcome before returning to idle.
    pub async fn finish_model_attempt(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
        outcome: ModelTerminalOutcome,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        let active = matches!(
            &self.phase,
            ConversationPhase::AwaitingModel {
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
            self.model_started = false;
            self.phase = ConversationPhase::PersistingAssistant {
                turn_id,
                attempt_id,
            };
            return Ok(vec![
                RuntimeEffect::CommitAssistant {
                    turn_id,
                    attempt_id,
                },
                RuntimeEffect::Emit(RuntimeEvent::ResponseFinished(outcome)),
            ]);
        }

        let next_revision = self
            .canonical_revision
            .checked_add(1)
            .ok_or(RuntimeError::IdExhausted)?;
        let (persisted_outcome, failure_details) = match &outcome {
            ModelTerminalOutcome::Interrupted(interruption) => (
                TurnOutcome::Interrupted {
                    reason: interruption.reason.clone(),
                },
                Some((
                    SessionErrorCategory::Model,
                    RuntimeFailureCategory::Model,
                    interruption.reason.clone(),
                )),
            ),
            ModelTerminalOutcome::Cancelled(cancellation) => (
                TurnOutcome::Cancelled {
                    reason: cancellation.reason.clone(),
                },
                Some((
                    SessionErrorCategory::Model,
                    RuntimeFailureCategory::Model,
                    cancellation.reason.clone(),
                )),
            ),
            ModelTerminalOutcome::Failed(failure) => (
                TurnOutcome::Failed {
                    message: failure.message.clone(),
                },
                Some((
                    match failure.kind {
                        harness_model_api::ModelFailureKind::Protocol => {
                            SessionErrorCategory::Protocol
                        }
                        _ => SessionErrorCategory::Model,
                    },
                    match failure.kind {
                        harness_model_api::ModelFailureKind::Protocol => {
                            RuntimeFailureCategory::Protocol
                        }
                        _ => RuntimeFailureCategory::Model,
                    },
                    failure.message.clone(),
                )),
            ),
            ModelTerminalOutcome::Completed(_) => unreachable!(),
        };
        let mut records = Vec::with_capacity(2);
        if let Some((category, _, message)) = &failure_details {
            records.push(SessionPayload::Error {
                turn_id,
                category: *category,
                message: message.clone(),
            });
        }
        records.push(SessionPayload::TurnFinished {
            turn_id,
            outcome: persisted_outcome,
        });
        self.phase = ConversationPhase::Cancelling {
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
        self.transient_reasoning_content.clear();
        self.transient_reasoning_summary.clear();
        self.transient_reasoning_encrypted = None;
        self.pending_interrupt = None;
        self.model_started = false;
        self.phase = ConversationPhase::Idle;

        let mut effects = Vec::with_capacity(3);
        if let Some((_, category, message)) = failure_details {
            effects.push(RuntimeEffect::Emit(RuntimeEvent::TranscriptAppended(
                TranscriptSnapshotEntry {
                    sequence: receipt.sequences.clone().next(),
                    payload: TranscriptPayload::Error { category, message },
                },
            )));
        }
        effects.push(RuntimeEffect::Emit(RuntimeEvent::ResponseFinished(outcome)));
        effects.push(RuntimeEffect::Emit(RuntimeEvent::AgenticLoopCompleted));
        Ok(effects)
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
                let start_model = self.start_model_request().await?;
                Ok(vec![
                    RuntimeEffect::Emit(RuntimeEvent::AgenticLoopStarted),
                    RuntimeEffect::Emit(RuntimeEvent::TranscriptAppended(
                        harness_runtime_api::TranscriptSnapshotEntry {
                            sequence: receipt.sequences.clone().last(),
                            payload: harness_runtime_api::TranscriptPayload::Message {
                                role: harness_runtime_api::MessageRole::User,
                                text: persisted_text,
                            },
                        },
                    )),
                    start_model,
                ])
            }
            RuntimeCommand::Retry => {
                if self.lifecycle != RuntimeLifecycle::Ready {
                    return Err(RuntimeError::InvalidLifecycle);
                }
                if self.phase != ConversationPhase::Idle {
                    return Err(RuntimeError::InvalidPhase);
                }

                let mut retry_turn = None;
                for payload in self.canonical_history.iter().rev() {
                    match payload {
                        // Errors and turn/attempt markers are bookkeeping around the
                        // actual turn and must not hide a retryable input or tool result.
                        SessionPayload::Error { .. }
                        | SessionPayload::TurnFinished { .. }
                        | SessionPayload::ModelAttemptStarted { .. }
                        | SessionPayload::ToolExecutionStarted { .. }
                        | SessionPayload::TurnStarted { .. } => {}
                        SessionPayload::InputMessage { turn_id, .. }
                        | SessionPayload::ToolCallAccepted { turn_id, .. }
                        | SessionPayload::ToolExecutionFinished { turn_id, .. } => {
                            retry_turn = Some(*turn_id);
                            break;
                        }
                        // A real assistant/reasoning/checkpoint record means the
                        // retryable turn is no longer the durable tail.
                        _ => break,
                    }
                }
                let turn_id = retry_turn.ok_or(RuntimeError::RetryUnavailable)?;

                self.request_loop_stopped = false;
                self.pending_interrupt = None;
                self.phase = ConversationPhase::PreparingAttempt { turn_id };
                Ok(vec![
                    RuntimeEffect::Emit(RuntimeEvent::AgenticLoopStarted),
                    RuntimeEffect::ContinueModel { turn_id },
                ])
            }
            RuntimeCommand::SetToolAvailability { pattern, enabled } => {
                if self.lifecycle != RuntimeLifecycle::Ready {
                    return Err(RuntimeError::InvalidLifecycle);
                }
                self.configuration
                    .ports
                    .tool_availability
                    .write()
                    .map_err(|_| RuntimeError::ToolAvailabilityUnavailable)?
                    .set(pattern, enabled)
                    .map_err(|_| RuntimeError::InvalidToolAvailabilityPattern)?;
                Ok(Vec::new())
            }
            RuntimeCommand::Compact { instruction } => {
                self.begin_compaction(instruction)?;
                Ok(vec![
                    RuntimeEffect::Emit(RuntimeEvent::CompactionStarted),
                    RuntimeEffect::Emit(RuntimeEvent::AgenticLoopStarted),
                    self.start_compaction_request()?,
                ])
            }
            RuntimeCommand::RetryCompaction { instruction } => {
                self.redo_compaction_with_instruction(instruction)?;
                Ok(vec![
                    RuntimeEffect::Emit(RuntimeEvent::CompactionStarted),
                    RuntimeEffect::Emit(RuntimeEvent::AgenticLoopStarted),
                    self.start_compaction_request()?,
                ])
            }
            RuntimeCommand::CancelCompaction => {
                self.cancel_compaction()?;
                Ok(Vec::new())
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
                        | ConversationPhase::PreparingContinuation { .. }
                ) {
                    return Err(RuntimeError::InvalidPhase);
                }
                self.queued_steering.push(text);
                Ok(vec![RuntimeEffect::Emit(RuntimeEvent::SteeringChanged(
                    Some(self.queued_steering.join("\n")),
                ))])
            }
            RuntimeCommand::StopRequestLoop => {
                if self.lifecycle != RuntimeLifecycle::Ready {
                    return Err(RuntimeError::InvalidLifecycle);
                }
                self.request_loop_stopped = true;
                Ok(Vec::new())
            }
            RuntimeCommand::AbortResponse => {
                if self.lifecycle != RuntimeLifecycle::Ready {
                    return Err(RuntimeError::InvalidLifecycle);
                }
                match &self.phase {
                    ConversationPhase::AwaitingModel { turn_id, attempt_id } => {
                        self.interrupt(*turn_id, *attempt_id)?;
                    }
                    ConversationPhase::Cancelling { .. } => {}
                    _ => {
                        self.request_loop_stopped = true;
                    }
                }
                Ok(Vec::new())
            }
            RuntimeCommand::Interrupt { text } => {
                if self.lifecycle != RuntimeLifecycle::Ready {
                    return Err(RuntimeError::InvalidLifecycle);
                }
                // Stash steering text (if any) for the next model attempt.
                if !text.is_empty() {
                    self.pending_interrupt = Some(text);
                }
                // Interrupt the active model attempt if one is awaiting. If the
                // runtime is between attempts (preparing, executing tools, or
                // persisting), the pending interrupt / stopped loop flag will
                // short-circuit the turn at the next continuation.
                let interrupted = match &self.phase {
                    ConversationPhase::AwaitingModel {
                        turn_id,
                        attempt_id,
                    } => {
                        self.interrupt(*turn_id, *attempt_id)?;
                        true
                    }
                    ConversationPhase::Cancelling { .. } => true,
                    _ => false,
                };
                if !interrupted {
                    // Signal the request loop to stop so the turn ends as soon
                    // as the current phase completes, rather than starting
                    // another model attempt.
                    self.request_loop_stopped = true;
                }
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
            RuntimeCommand::SetGoal { instruction } => {
                if self.lifecycle != RuntimeLifecycle::Ready {
                    return Err(RuntimeError::InvalidLifecycle);
                }
                if instruction.is_empty() {
                    return Err(RuntimeError::EmptyCommandText);
                }
                self.enable_persist(instruction, CompletionPolicy::ModelMayComplete)?;
                Ok(Vec::new())
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
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
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

        let goal_completion = call.name == "goal"
            && is_goal_completion_input(call.input.as_str())
            && matches!(
                &self.persist_state,
                PersistState::Active(task)
                    if task.completion_policy == CompletionPolicy::ModelMayComplete
            );
        let records = if goal_completion {
            vec![SessionPayload::ToolCallAccepted {
                turn_id,
                call_id: call_id.clone(),
                name: call.name.clone(),
                input: call.input.as_str().to_owned(),
            }]
        } else {
            vec![
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
            ]
        };
        self.phase = ConversationPhase::PersistingToolCall {
            turn_id,
            call_id: call_id.clone(),
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
        self.pending_tool_calls.remove(0);
        let transcript = RuntimeEffect::Emit(RuntimeEvent::TranscriptAppended(
            harness_runtime_api::TranscriptSnapshotEntry {
                sequence: None,
                payload: harness_runtime_api::TranscriptPayload::ToolCall {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                },
            },
        ));
        if goal_completion {
            self.active_tool_call = None;
            self.phase = ConversationPhase::PreparingContinuation { turn_id };
            self.complete_persist()?;
            self.complete_turn(turn_id).await?;
            return Ok(vec![
                transcript,
                RuntimeEffect::Emit(RuntimeEvent::AgenticLoopCompleted),
            ]);
        }
        self.active_tool_call = Some(call);
        self.phase = ConversationPhase::ExecutingTool {
            turn_id,
            execution_id,
            call_id: call_id.clone(),
        };
        let request = self.active_tool_execution_request(ToolExecutionPolicy {
            deadline_ms: 30_000,
            cancellable: true,
        })?;
        Ok(vec![
            transcript,
            RuntimeEffect::ExecuteTool {
                turn_id,
                call_id,
                request,
            },
        ])
    }

    /// Builds an execution request for the durably accepted active tool call.
    /// Returns the configured tool executor for composition-root orchestration.
    pub fn tool_executor(&self) -> Result<Arc<dyn ToolExecutor>, RuntimeError> {
        self.configuration
            .ports
            .tool_executor
            .as_ref()
            .cloned()
            .ok_or(RuntimeError::ToolExecutorUnavailable)
    }

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
        let advertised = self
            .configuration
            .ports
            .tool_registry
            .get(&tool)
            .ok_or(RuntimeError::UnknownTool)?;
        Ok(ToolExecutionRequest {
            execution_id: ToolExecutionId(execution_id),
            tool,
            route: advertised.executor.clone(),
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
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
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
            output: output.clone(),
        }];
        self.phase = ConversationPhase::PersistingToolResult {
            turn_id,
            execution_id,
            call_id: call_id.clone(),
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
        self.active_tool_call = None;
        let transcript = RuntimeEffect::Emit(RuntimeEvent::TranscriptAppended(
            harness_runtime_api::TranscriptSnapshotEntry {
                sequence: None,
                payload: harness_runtime_api::TranscriptPayload::ToolResult {
                    call_id,
                    output,
                },
            },
        ));
        if self.pending_tool_calls.is_empty() {
            self.phase = ConversationPhase::PreparingAttempt { turn_id };
            Ok(vec![transcript, RuntimeEffect::ContinueModel { turn_id }])
        } else {
            self.phase = ConversationPhase::PreparingContinuation { turn_id };
            Ok(vec![transcript, RuntimeEffect::ContinueTurn { turn_id }])
        }
    }

    /// Continues a turn after the assistant response or one tool result is durable.
    pub async fn continue_turn(
        &mut self,
        turn_id: u64,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        if self.phase != (ConversationPhase::PreparingContinuation { turn_id }) {
            return Err(RuntimeError::InvalidPhase);
        }
        if self.request_loop_stopped {
            self.pending_tool_calls.clear();
            self.active_tool_call = None;
            self.queued_steering.clear();
            self.complete_turn(turn_id).await?;
            return Ok(vec![RuntimeEffect::Emit(RuntimeEvent::AgenticLoopCompleted)]);
        }
        if self.pending_tool_calls.is_empty() {
            if !self.queued_steering.is_empty() {
                self.phase = ConversationPhase::PreparingAttempt { turn_id };
                return Ok(vec![RuntimeEffect::ContinueModel { turn_id }]);
            }
            // Keep the agent loop running when a goal is active so the model
            // can work autonomously until it calls `goal complete`.
            // Inject a continuation user message (via steering) to prevent
            // consecutive assistant messages, which the Chat Completions API
            // rejects. Mirrors the old harness-core approach of appending a
            // durable continuation prompt before each re-request.
            if let PersistState::Active(task) = &self.persist_state {
                self.queued_steering.push(format!(
                    "Persist mode is active.\n\nPersisted task:\n{}\n\n\
                     Continue working on the persisted task after this response \
                     completion. Do not stop just because a response is done. \
                     Before calling the `goal` tool, verify the persisted \
                     task's completion criteria against the conversation and \
                     observed results. Only call `goal` with exactly \
                     `complete` after those criteria are satisfied; that tool \
                     has no output and ends persist mode.",
                    task.instruction,
                ));
                self.phase = ConversationPhase::PreparingAttempt { turn_id };
                return Ok(vec![RuntimeEffect::ContinueModel { turn_id }]);
            }
            self.complete_turn(turn_id).await?;
            return Ok(vec![RuntimeEffect::Emit(RuntimeEvent::AgenticLoopCompleted)]);
        }
        self.accept_next_tool_call(turn_id).await
    }

    /// Loads one older transcript page through the injected read port.
    pub fn load_transcript_page(
        &self,
        session_id: harness_session_store::SessionId,
        before_sequence: Option<u64>,
        page_size: PageSize,
    ) -> Result<RuntimeEffect, RuntimeError> {
        if session_id != self.configuration.session_id {
            return Err(RuntimeError::InvalidLifecycle);
        }
        let page = self
            .configuration
            .ports
            .session_store
            .reader()?
            .load_older(session_id, before_sequence, page_size)?;
        Ok(self.apply_transcript_page(page))
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
                    SessionPayload::Reasoning {
                        summary, content, ..
                    } => {
                        let text = summary.or(content).unwrap_or_default();
                        if text.is_empty() {
                            return None;
                        }
                        TranscriptPayload::Thinking { text }
                    }
                    SessionPayload::Error {
                        category, message, ..
                    } => TranscriptPayload::Error {
                        category: match category {
                            SessionErrorCategory::Model => RuntimeFailureCategory::Model,
                            SessionErrorCategory::Protocol => RuntimeFailureCategory::Protocol,
                            SessionErrorCategory::Tool => RuntimeFailureCategory::Tool,
                            SessionErrorCategory::Lifecycle => RuntimeFailureCategory::Lifecycle,
                        },
                        message,
                    },
                    SessionPayload::ToolExecutionFinished {
                        call_id, output, ..
                    } => TranscriptPayload::ToolResult { call_id, output },
                    SessionPayload::CompactionCheckpoint { summary, .. } => {
                        TranscriptPayload::Thinking { text: summary }
                    }
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

    /// Begins a staged compaction against the current canonical history.
    pub fn begin_compaction(
        &mut self,
        instruction: String,
    ) -> Result<(), RuntimeError> {
        if self.lifecycle != RuntimeLifecycle::Ready {
            return Err(RuntimeError::InvalidLifecycle);
        }
        if self.phase != ConversationPhase::Idle {
            return Err(RuntimeError::InvalidPhase);
        }
        if self.canonical_history.is_empty() {
            return Err(RuntimeError::CompactionEmptyHistory);
        }
        if self.compaction.is_some() {
            return Err(RuntimeError::CompactionAlreadyRunning);
        }
        let source = compaction::CompactionSource {
            revision: self.canonical_revision,
            history: self.canonical_history.clone(),
        };
        self.compaction = Some(
            compaction::CompactionCoordinator::begin(source, instruction)
                .map_err(|_| RuntimeError::CompactionAlreadyRunning)?,
        );
        Ok(())
    }

    /// Returns whether a compaction model attempt is active.
    pub fn compaction_attempt_active(&self) -> bool {
        matches!(self.phase, ConversationPhase::Compacting { .. })
    }

    /// Retries the active model attempt using function-tool compatibility encoding.
    pub fn retry_with_compatibility(
        &mut self,
        turn_id: u64,
        attempt_id: u64,
    ) -> Result<Option<RuntimeEffect>, RuntimeError> {
        if self.compatibility_mode {
            return Ok(None);
        }
        match self.phase {
            ConversationPhase::AwaitingModel {
                turn_id: active_turn,
                attempt_id: active_attempt,
            } if active_turn == turn_id && active_attempt == attempt_id => {}
            _ => return Err(RuntimeError::InvalidPhase),
        }
        let request = self.configuration.ports.request_builder.build_compatibility(
            self.canonical_revision,
            &self.configuration.model,
            self.configuration.ports.model_route.generation,
            &self.canonical_history,
            &[],
        )?;
        if request.provider_generation != self.configuration.ports.model_route.generation {
            return Err(RuntimeError::ProviderGenerationMismatch);
        }
        if request.history_revision != self.canonical_revision {
            return Err(RuntimeError::RequestRevisionMismatch);
        }
        self.compatibility_mode = true;
        self.model_started = false;
        Ok(Some(RuntimeEffect::StartModel {
            turn_id,
            attempt: Arc::new(ModelAttempt::initial(
                request,
                harness_model_api::ModelAttemptId(attempt_id),
            )),
            route: self.configuration.ports.model_route.clone(),
        }))
    }

    /// Returns whether the active model response emitted its start event.
    pub fn model_response_started(&self) -> bool {
        self.model_started
    }

    /// Returns a failed compaction attempt to the idle state while retaining its source.
    pub fn fail_compaction_attempt(&mut self) {
        if self.compaction_attempt_active() {
            self.phase = ConversationPhase::Idle;
            self.model_started = false;
        }
    }

    /// Adds one streamed compaction fragment to the staged result.
    pub fn record_compaction_delta(&mut self, delta: &str) -> Result<(), RuntimeError> {
        self.compaction
            .as_mut()
            .ok_or(RuntimeError::CompactionNotActive)?
            .push_delta(delta)
            .map_err(|_| RuntimeError::CompactionNotActive)
    }

    /// Validates the staged compaction response without changing active history.
    pub fn finish_compaction(&mut self) -> Result<(), RuntimeError> {
        self.compaction
            .as_mut()
            .ok_or(RuntimeError::CompactionNotActive)?
            .finish()
            .map(|_| ())
            .map_err(|_| RuntimeError::CompactionInvalidResult)
    }

    /// Retries compaction from the preserved pre-compaction source.
    pub fn redo_compaction(&mut self) -> Result<(), RuntimeError> {
        self.redo_compaction_with_instruction(None)
    }

    /// Retries compaction with optional replacement instructions.
    pub fn redo_compaction_with_instruction(
        &mut self,
        instruction: Option<String>,
    ) -> Result<(), RuntimeError> {
        self.compaction
            .as_mut()
            .ok_or(RuntimeError::CompactionNotActive)?
            .redo_with_instruction(instruction)
            .map_err(|_| RuntimeError::CompactionRedoUnavailable)
    }

    /// Cancels staged compaction and preserves the active canonical history.
    pub fn cancel_compaction(&mut self) -> Result<(), RuntimeError> {
        let coordinator = self
            .compaction
            .as_mut()
            .ok_or(RuntimeError::CompactionNotActive)?;
        coordinator.cancel();
        self.compaction = None;
        Ok(())
    }

    /// Returns whether a compaction transaction is staged.
    pub fn has_compaction(&self) -> bool {
        self.compaction.is_some()
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

    /// Reports whether a completed model response queued tool calls.
    pub fn has_pending_tool_calls(&self) -> bool {
        !self.pending_tool_calls.is_empty()
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

    /// Returns the currently active model selection.
    pub fn active_model(&self) -> &harness_model_api::ModelSelection {
        &self.configuration.model
    }

    /// Returns the currently active model transport.
    pub fn active_transport(&self) -> std::sync::Arc<dyn harness_model_api::ModelTransport> {
        std::sync::Arc::clone(&self.configuration.ports.model_transport)
    }

    /// Update the transport and root model route ports in-place.
    pub fn update_ports(
        &mut self,
        model_transport: std::sync::Arc<dyn harness_model_api::ModelTransport>,
        model_route: harness_model_api::ResolvedModelRoute,
    ) {
        self.configuration.ports.model_transport = model_transport;
        self.configuration.ports.model_route = model_route;
    }

    /// Updates the route used for future compaction requests.
    pub fn update_compaction_route(&mut self, route: harness_model_api::ResolvedModelRoute) {
        self.configuration.ports.compaction_route = route;
    }

    /// Appends records directly to the session store.
    pub async fn append_records(
        &mut self,
        records: &[SessionPayload],
    ) -> Result<harness_session_store::AppendReceipt, RuntimeError> {
        let writer = self.writer.as_mut().ok_or(RuntimeError::InvalidLifecycle)?;
        writer
            .append(records, Durability::Durable)
            .await
            .map_err(RuntimeError::Session)
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
    /// Tool execution was requested without an executor.
    #[error("tool execution is unavailable")]
    ToolExecutorUnavailable,
    /// The tool registry cannot be read because its lock is poisoned.
    #[error("tool registry is unavailable")]
    ToolRegistryUnavailable,
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
    /// The session has no durable history to compact.
    #[error("session history is empty; nothing to compact")]
    CompactionEmptyHistory,
    /// A compaction transaction is already staged.
    #[error("compaction is already running")]
    CompactionAlreadyRunning,
    /// No compaction transaction is staged.
    #[error("compaction is not active")]
    CompactionNotActive,
    /// The staged model output failed compaction validation.
    #[error("compaction result is invalid")]
    CompactionInvalidResult,
    /// The preserved source is no longer available for redo.
    #[error("compaction cannot be redone")]
    CompactionRedoUnavailable,
    /// The dynamic tool-availability policy cannot be read or written.
    #[error("tool availability policy is unavailable")]
    ToolAvailabilityUnavailable,
    /// A dynamic tool-availability pattern is invalid.
    #[error("tool availability pattern is invalid")]
    InvalidToolAvailabilityPattern,
    /// The durable tail does not identify a user or tool turn that can be retried.
    #[error("the last durable message cannot be retried")]
    RetryUnavailable,
}

fn is_goal_completion_input(input: &str) -> bool {
    let input = input.trim();
    input == "complete"
        || input == r#"{"input":"complete"}"#
        || input == r#"{ "input": "complete" }"#
}

/// Port that builds immutable model requests from canonical state.
pub trait ModelRequestBuilder: Send + Sync {
    /// Builds a request for the current canonical revision and selected model.
    /// Builds immutable semantic model requests.
    fn build(
        &self,
        revision: u64,
        selection: &ModelSelection,
        provider_generation: ProviderGeneration,
        history: &[SessionPayload],
        steering: &[String],
    ) -> Result<Arc<ModelRequest>, RuntimeError>;

    /// Builds a request using function-tool compatibility encoding.
    fn build_compatibility(
        &self,
        revision: u64,
        selection: &ModelSelection,
        provider_generation: ProviderGeneration,
        history: &[SessionPayload],
        steering: &[String],
    ) -> Result<Arc<ModelRequest>, RuntimeError> {
        self.build(revision, selection, provider_generation, history, steering)
    }

    /// Builds a request whose response is a compaction summary.
    fn build_compaction(
        &self,
        revision: u64,
        selection: &ModelSelection,
        provider_generation: ProviderGeneration,
        history: &[SessionPayload],
        instruction: &str,
    ) -> Result<Arc<ModelRequest>, RuntimeError> {
        self.build(
            revision,
            selection,
            provider_generation,
            history,
            &[instruction.to_owned()],
        )
    }
}

/// Effect scheduled by a reducer transition.
#[derive(Debug)]
pub enum RuntimeEffect {
    /// Start one normal model attempt.
    StartModel {
        /// Turn owning the attempt.
        turn_id: u64,
        /// Attempt to submit.
        attempt: Arc<ModelAttempt>,
        /// Route selected for this attempt.
        route: ResolvedModelRoute,
    },
    /// Start one compaction model attempt.
    StartCompaction {
        /// Compaction identity.
        compaction_id: u64,
        /// Attempt to submit.
        attempt: Arc<ModelAttempt>,
        /// Compaction route.
        route: ResolvedModelRoute,
    },
    /// Commit a validated compaction checkpoint.
    CommitCompaction {
        /// Compaction identity.
        compaction_id: u64,
        /// Validated summary text.
        summary: String,
    },
    /// Commit one completed assistant response.
    CommitAssistant {
        /// Turn owning the response.
        turn_id: u64,
        /// Attempt that produced the response.
        attempt_id: u64,
    },
    /// Continue a turn after durable assistant or tool state.
    ContinueTurn {
        /// Turn to continue.
        turn_id: u64,
    },
    /// Start another model attempt after durable tool output.
    ContinueModel {
        /// Turn owning the next attempt.
        turn_id: u64,
    },
    /// Execute one durably accepted tool call.
    ExecuteTool {
        /// Turn owning the tool call.
        turn_id: u64,
        /// Model call identifier.
        call_id: String,
        /// Request prepared by the reducer.
        request: ToolExecutionRequest,
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
