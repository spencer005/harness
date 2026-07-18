use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    fs,
    io::{BufReader, Read, Seek, SeekFrom},
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use fff_search::{
    FFFMode, FilePicker, FilePickerOptions, GrepMode, GrepSearchOptions, QueryParser,
};
use object::{
    Object as _, ObjectSection as _, ObjectSegment as _, ObjectSymbol as _, ObjectSymbolTable as _,
    read::elf::{Dyn as _, ProgramHeader as _},
};
use thiserror::Error;
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_BLUE: &str = "\x1b[34m";

const INSPECT_COMMAND_MATCH_LIMIT: usize = 10;
const INSPECT_BYTES_MAX_LENGTH: usize = 16_384;
const INSPECT_LIST_ENTRY_LIMIT: usize = 500;
const INSPECT_INDEX_RESULT_LIMIT: usize = 100;
const INSPECT_STRING_PREVIEW_CHARS: usize = 160;

use uuid::Uuid;

use crate::{
    actors::{
        ActorHandle, ActorReceiver, ActorSender, DEFAULT_ACTOR_MAILBOX_CAPACITY, RuntimeCommand,
        RuntimeEvent, channel as actor_channel,
    },
    analytics::{CodexAnalytics, TurnEventContext},
    commands::CommandCatalog,
    compact::{
        CompactRequest, CompactResult, CompactionPlan, CompactionTrigger, ContextWindowDecision,
        ContextWindowPolicy, estimate_text_tokens, plan_compaction,
    },
    provider_runtime::{
        ProviderError, ProviderRuntimeBuilder, ProviderTestMode, test_provider_profile,
    },
    provider_transport::{ProviderCommand, provider_transport_for_runtime},
    providers::ProviderProfileId,
    responses::{
        ModelSettings, ResponsesCommand, ResponsesCreateRequest, ResponsesEvent, ResponsesHeaders,
        ResponsesModelCapabilities, ResponsesModelInfo, ResponsesRequest,
        ResponsesRequestBuildError, ResponsesRequestId, context_window_policy_from_model_info,
        normalize_model_slug, parse_reasoning_effort, parse_service_tier,
    },
    sessions::{
        CompactionCheckpointRecord, FreeformToolCallRecord, FreeformToolInputDeltaRecord,
        FunctionToolCallRecord, HistoryRecord, InspectReadDisplayRecord, InspectReadNextRecord,
        MessageRecord, SessionActor, SessionCommand, SessionEvent, SessionId, SessionMeta,
        SessionRecordKind, ToolOutputDisplayRecord, TranscriptPage, TurnContextRecord,
    },
    steering::{SteeringMode, SteeringQueue, append_queued_steering_text},
    subagents::{AgentId, AgentScheduler, AgentSchedulerCommand, AgentSchedulerEvent},
    terminal::TerminalManager,
    tools::{
        FreeformToolCall, FunctionToolCall, INSPECT_TOOL_NAME, MARK_TASK_COMPLETE_TOOL_NAME,
        NativeTool, NativeToolExecutionOutput, NativeToolHandler, NativeToolRegistry,
    },
};

mod compaction_prompts;
mod compaction_result;
mod error_classification;
mod persist_mode;
mod responses_input;
mod root_prompts;
mod slash_commands;
mod text;
mod tool_output_summary;
mod tool_records;

use compaction_prompts::{
    auto_compaction_prompt, compaction_instructions, context_error_compaction_prompt,
    manual_compaction_prompt,
};
use compaction_result::{
    compact_result_from_text, compaction_checkpoint_text, normalized_compaction_summary,
};
use error_classification::{
    is_context_window_error, is_reconnect_required_response_error, is_retryable_response_error,
};
use persist_mode::{PersistCommandAction, persist_command_action};
use responses_input::{
    MessageInputRole, assistant_text_delta_from_frame, assistant_text_from_frame,
    history_from_session_records, history_to_responses_input, message_input, preview_from_message,
    response_event_request_id, title_from_message, utc_date_from_unix_ms,
};
use root_prompts::{
    instructions_with_cwd, persist_continuation_message, root_queued_steering_message,
};
use slash_commands::{SlashCommand, slash_command_catalog};
use tool_output_summary::{
    normalized_tool_output_summary, pending_tool_output_summary_parts, recent_tool_summary_context,
    summarized_tool_output, tool_output_needs_summary, tool_output_summary_failure_output,
    tool_output_summary_instructions, tool_output_summary_prompt,
};
use tool_records::{freeform_tool_output_record, function_tool_output_record};

/// Default input routing mode for new harness sessions.
pub const DEFAULT_DEVELOPER_MODE: bool = false;

static NEXT_COMPACTION_ID_NONCE: AtomicU64 = AtomicU64::new(1);
static NEXT_LOCATE_AGENT_ID: AtomicU64 = AtomicU64::new(1);

const TOOL_OUTPUT_SUMMARY_TARGET_TOKENS: u64 = 2_000;
const TOOL_OUTPUT_SUMMARY_THRESHOLD_TOKENS: u64 = 16_000;
const TOOL_OUTPUT_SUMMARY_PROMPT_MAX_TOKENS: u64 = 30_000;
const FFF_MODEL_OUTPUT_MAX_TOKENS: u64 = 30_000;
const FFF_TOTAL_MATCH_LIMIT: usize = 100;
const TOOL_OUTPUT_SUMMARY_CONTEXT_RECORDS: usize = 6;
const TOOL_OUTPUT_SUMMARY_MAX_ATTEMPTS: u8 = 10;

#[derive(Debug, Error)]
enum CompactionRequestError {
    #[error("no active session to compact")]
    NoActiveSession,
    #[error("session history is empty; nothing to compact")]
    EmptyHistory,
    #[error("failed to encode compaction history item: {0}")]
    HistoryInput(#[from] sonic_rs::Error),
    #[error("failed to encode compaction prompt message: {0}")]
    PromptMessage(sonic_rs::Error),
    #[error("failed to build compaction Responses request: {0}")]
    Request(#[from] ResponsesRequestBuildError),
}

#[derive(Debug, Error)]
enum ToolOutputSummaryError {
    #[error("failed to encode tool-output summary prompt message: {0}")]
    PromptMessage(sonic_rs::Error),
    #[error("failed to build tool-output summary Responses request: {0}")]
    Request(#[from] ResponsesRequestBuildError),
    #[error("responses actor error during tool output summary: {0}")]
    ResponsesActor(String),
}

#[derive(Debug, Error)]
enum RootRequestBuildError {
    #[error("failed to encode root conversation history")]
    HistoryInput {
        #[from]
        source: sonic_rs::Error,
    },
    #[error("failed to build root Responses request")]
    Request {
        #[from]
        source: ResponsesRequestBuildError,
    },
}

#[derive(Debug, Error)]
enum ActorRequestError {
    #[error("{actor} actor command mailbox failed")]
    Send {
        actor: &'static str,
        #[source]
        source: crate::actors::ActorSendError,
    },
    #[error("{actor} actor reply mailbox closed")]
    Receive {
        actor: &'static str,
        #[source]
        source: crossfire::RecvError,
    },
}

#[derive(Debug, Error)]
enum SessionOperationError {
    #[error(transparent)]
    Request(#[from] ActorRequestError),
    #[error("session actor error: {message}")]
    Actor { message: String },
    #[error("unexpected session event: {event:?}")]
    Unexpected { event: SessionEvent },
}

/// Runtime configuration for the root harness actor.
#[derive(Clone)]
pub struct HarnessConfig {
    /// Directory containing session files and indexes.
    pub session_root: PathBuf,
    /// Durable id for this root conversation session.
    pub session_id: SessionId,
    /// Existing session file to resume before accepting new input.
    pub resume_session_path: Option<PathBuf>,
    /// Oldest transcript sequence already present in the initial UI snapshot.
    pub initial_transcript_before_seq: Option<u64>,
    /// Working directory for turns in this runtime.
    pub cwd: String,
    /// Validated provider runtime used for all turns.
    pub provider_runtime: crate::provider_runtime::ProviderRuntime,
    /// Current provider profile summary.
    pub provider: crate::providers::ProviderUiInfo,
    /// Provider config store used for runtime profile switching.
    pub provider_store: Option<Arc<dyn crate::providers::ProviderConfigStore>>,
    /// Credential store used to resolve API-key provider auth.
    pub credential_store: Option<Arc<dyn crate::provider_runtime::ProviderCredentialStore>>,
    /// Codex home path used to resolve ChatGPT auth; `None` uses the default.
    pub codex_home: Option<PathBuf>,
    /// Model slug used for Responses requests.
    pub model: String,
    /// Reasoning effort sent to the Responses API.
    pub reasoning_effort: Option<String>,
    /// Optional Responses service tier; Codex Fast mode is `priority`.
    pub service_tier: Option<String>,
    /// When enabled, non-slash input is sent as Responses developer-role messages.
    pub developer_mode: bool,
    /// System/developer instructions sent in each request.
    pub instructions: String,
    /// Source label persisted in session metadata.
    pub source: String,
    /// Originator label persisted in session metadata.
    pub originator: String,
    /// Client version persisted in session metadata.
    pub client_version: String,
    /// Sandbox policy persisted in turn context records.
    pub sandbox_policy: String,
    /// Approval policy persisted in turn context records.
    pub approval_policy: String,
    /// Headers used by the Responses WebSocket actor.
    pub responses_headers: ResponsesHeaders,
    /// Model capability metadata used for request construction.
    pub model_capabilities: ResponsesModelCapabilities,
    /// Catalog returned by `/models`, used for `/model` settings commands.
    pub model_catalog: Vec<ResponsesModelInfo>,
    /// Native tools available to the model and their in-process handlers.
    pub tool_registry: NativeToolRegistry,
    /// Whether PTY terminal tools are advertised to the model.
    pub terminal_tools_enabled: bool,
    /// Optional context-window policy used to compact history before submitting a request.
    pub context_window_policy: Option<ContextWindowPolicy>,
    /// Fire-and-forget analytics emitter; disabled when `None`.
    pub analytics: Option<CodexAnalytics>,
}

impl std::fmt::Debug for HarnessConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessConfig")
            .field("session_root", &self.session_root)
            .field("session_id", &self.session_id)
            .field("resume_session_path", &self.resume_session_path)
            .field(
                "initial_transcript_before_seq",
                &self.initial_transcript_before_seq,
            )
            .field("cwd", &self.cwd)
            .field("provider_runtime", &self.provider_runtime)
            .field("provider", &self.provider)
            .field(
                "provider_store",
                &self.provider_store.as_ref().map(|_| "..."),
            )
            .field(
                "credential_store",
                &self.credential_store.as_ref().map(|_| "..."),
            )
            .field("codex_home", &self.codex_home)
            .field("model", &self.model)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("service_tier", &self.service_tier)
            .field("developer_mode", &self.developer_mode)
            .field("instructions", &self.instructions)
            .field("source", &self.source)
            .field("originator", &self.originator)
            .field("client_version", &self.client_version)
            .field("sandbox_policy", &self.sandbox_policy)
            .field("approval_policy", &self.approval_policy)
            .field("responses_headers", &self.responses_headers)
            .field("model_capabilities", &self.model_capabilities)
            .field("model_catalog", &self.model_catalog)
            .field("tool_registry", &self.tool_registry)
            .field("terminal_tools_enabled", &self.terminal_tools_enabled)
            .field("context_window_policy", &self.context_window_policy)
            .field("analytics", &self.analytics)
            .finish()
    }
}

/// Running harness endpoints for UI clients.
#[derive(Debug)]
pub struct HarnessRuntime {
    /// Command mailbox for the harness actor.
    pub commands: ActorHandle<RuntimeCommand>,
    /// UI/runtime event stream emitted by the harness actor.
    pub events: ActorReceiver<RuntimeEvent>,
}

/// Conversation orchestrator that sits between UI, sessions, and WS requests.
pub struct HarnessActor {
    config: HarnessConfig,
    slash_commands: CommandCatalog<SlashCommand>,
    root_persist_active: bool,
    root_persist_task: Option<String>,
    root_persist_paused: bool,
    root_agentic_loop_active: bool,
    pending_provider_profile_id: Option<ProviderProfileId>,
    session: ActorHandle<SessionCommand>,
    agent_scheduler: ActorHandle<AgentSchedulerCommand>,
    responses: ActorHandle<ProviderCommand>,
    rx: ActorReceiver<RuntimeCommand>,
    events: ActorSender<RuntimeEvent>,
    agent_events_rx: ActorReceiver<AgentSchedulerEvent>,
    response_events_tx: ActorSender<ResponsesEvent>,
    response_events_rx: ActorReceiver<ResponsesEvent>,
    active_session: Option<ActiveSession>,
    active_compaction_streams: HashMap<ResponsesRequestId, ActiveCompactionStream>,
    active_compaction_groups: HashMap<CompactionGroupId, ActiveCompactionGroup>,
    ignored_compaction_request_ids: HashSet<ResponsesRequestId>,
    active_tool_output_summaries: HashMap<ResponsesRequestId, ActiveToolOutputSummary>,
    /// Transport for the ollama-cloud provider, lazily initialized and used by
    /// the summarizer and locate worker when the primary provider is not
    /// ollama-cloud. `None` means either the primary provider is ollama-cloud,
    /// or we have not yet tried to build it (or it failed to build).
    ollama_cloud_responses: Option<ActorHandle<ProviderCommand>>,
    /// Validated ollama-cloud runtime cached alongside the secondary transport.
    ollama_cloud_runtime: Option<crate::provider_runtime::ProviderRuntime>,
    root_response_state: RootResponseState,
    root_steering_queue: SteeringQueue,
    ignored_response_request_ids: HashSet<ResponsesRequestId>,
    pending_root_continuation: Option<PendingRootContinuation>,
    pending_assistant_text: String,
    pending_freeform_tool_inputs: HashMap<String, String>,
    pending_function_tool_arguments: HashMap<String, String>,
    staged_patches: HashMap<String, StagedPatch>,
    terminal: TerminalManager,
    turn_count: u64,
}

#[derive(Debug)]
struct ActiveSession {
    path: PathBuf,
    history: Vec<HistoryRecord>,
    oldest_live_transcript_seq: Option<u64>,
}

#[derive(Debug, Clone)]
struct StagedPatch {
    patch: String,
}

#[derive(Debug)]
enum RootResponseState {
    Idle,
    Submitted { request_id: ResponsesRequestId },
    Streaming(ActiveResponseStream),
}

#[derive(Debug)]
struct ActiveResponseStream {
    request_id: ResponsesRequestId,
    started_at: Instant,
    first_token_seen: bool,
}

#[derive(Debug, Clone, Copy)]
struct RetiredRootResponse {
    request_id: ResponsesRequestId,
    was_streaming: bool,
}

#[derive(Debug, Clone, Copy)]
struct RootTransportIdle;

#[derive(Debug)]
enum PendingRootContinuation {
    Submit,
    CompactCurrentSession {
        prompt: String,
        trigger: CompactionTrigger,
    },
    CompactRequest {
        trigger: CompactionTrigger,
        request: CompactRequest,
        attempt: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolsOverrideMode {
    Custom,
    Compat,
}

impl ToolsOverrideMode {
    fn tool_registry(self) -> NativeToolRegistry {
        match self {
            Self::Custom => NativeToolRegistry::codex(),
            Self::Compat => NativeToolRegistry::ollama_cloud(),
        }
    }

    fn status_label(self) -> &'static str {
        match self {
            Self::Custom => "custom",
            Self::Compat => "compat",
        }
    }
}

fn tools_override_mode_for_registry(registry: &NativeToolRegistry) -> ToolsOverrideMode {
    if registry.advertises_function_tool("terminal_open") {
        ToolsOverrideMode::Compat
    } else {
        ToolsOverrideMode::Custom
    }
}

#[derive(Debug)]
struct ActiveToolOutputSummary {
    pending_text: String,
    context: String,
    attempt: u8,
    transport: ActorHandle<ProviderCommand>,
    settings: ModelSettings,
    pending: PendingToolOutputSummary,
}

#[derive(Debug)]
enum PendingToolOutputSummary {
    RootFreeform {
        call_record: FreeformToolCallRecord,
        output: NativeToolExecutionOutput,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CompactionGroupId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveCompactionBlock {
    Single,
    Older,
    Newer,
}

#[derive(Debug)]
struct ActiveCompactionStream {
    group_id: CompactionGroupId,
    block: ActiveCompactionBlock,
    pending_text: String,
}

#[derive(Debug)]
struct ActiveCompactionGroup {
    trigger: CompactionTrigger,
    original_request: CompactRequest,
    plan: CompactionPlan,
    older_result: Option<CompactResult>,
    newer_result: Option<CompactResult>,
    attempt: u8,
}

impl HarnessActor {
    /// Spawn a root harness with an internal session actor and external WS actor.
    pub fn spawn(
        config: HarnessConfig,
        responses: ActorHandle<ResponsesCommand>,
    ) -> HarnessRuntime {
        let responses = crate::provider_transport::ProviderActor::wrap(responses);
        let session = SessionActor::spawn(config.session_root.clone());
        let (agent_events_handle, agent_events_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
        let agent_scheduler = AgentScheduler::spawn(agent_events_handle.sender());
        let (commands, rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
        let (events_handle, events) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
        let (response_events_handle, response_events_rx) =
            actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
        let actor = Self {
            config,
            slash_commands: slash_command_catalog(),
            root_persist_active: false,
            root_persist_task: None,
            root_persist_paused: false,
            root_agentic_loop_active: false,
            pending_provider_profile_id: None,
            session,
            agent_scheduler,
            responses,
            rx,
            events: events_handle.sender(),
            agent_events_rx,
            response_events_tx: response_events_handle.sender(),
            response_events_rx,
            active_session: None,
            active_compaction_streams: HashMap::new(),
            active_compaction_groups: HashMap::new(),
            ignored_compaction_request_ids: HashSet::new(),
            active_tool_output_summaries: HashMap::new(),
            ollama_cloud_responses: None,
            ollama_cloud_runtime: None,
            root_response_state: RootResponseState::Idle,
            root_steering_queue: SteeringQueue::default(),
            ignored_response_request_ids: HashSet::new(),
            pending_root_continuation: None,
            pending_assistant_text: String::new(),
            pending_freeform_tool_inputs: HashMap::new(),
            pending_function_tool_arguments: HashMap::new(),
            staged_patches: HashMap::new(),
            terminal: TerminalManager::new(),
            turn_count: 0,
        };
        let _ = actor
            .events
            .try_send(RuntimeEvent::ProviderChanged(actor.config.provider.clone()));
        actor.fire_thread_initialized();
        tokio::spawn(actor.run());
        HarnessRuntime { commands, events }
    }

    async fn run(mut self) {
        self.resume_startup_session().await;
        loop {
            tokio::select! {
                command = self.rx.recv() => {
                    match command {
                        Ok(RuntimeCommand::SubmitInput { text }) => self.handle_input(text).await,
                        Ok(RuntimeCommand::SetModelSettings { settings }) => self.apply_model_settings(settings).await,
                        Ok(RuntimeCommand::SubmitResponsesRequest { request }) => self.submit_responses_request(request).await,
                        Ok(RuntimeCommand::LoadTranscriptPage { before_seq, max_lines }) => {
                            self.load_transcript_page(before_seq, max_lines).await;
                        }
                        Ok(RuntimeCommand::QueueSteering { text }) => self.queue_root_steering(text).await,
                        Ok(RuntimeCommand::ApplySteering { text, mode }) => {
                            self.apply_root_steering(text, mode).await;
                        }
                        Ok(RuntimeCommand::Compact { request }) => {
                            self.compact_with_request(
                                CompactionTrigger::ContextWindowPressure,
                                request,
                            )
                            .await;
                        }
                        Ok(RuntimeCommand::Shutdown) | Err(_) => break,
                    }
                }
                response = self.response_events_rx.recv() => {
                    match response {
                        Ok(event) => self.handle_response_event(event).await,
                        Err(_) => {
                            self.emit_error(
                                "responses event channel closed unexpectedly".to_string(),
                            )
                            .await;
                            break;
                        }
                    }
                }
                agent_event = self.agent_events_rx.recv() => {
                    match agent_event {
                        Ok(event) => self.handle_agent_scheduler_event(event).await,
                        Err(_) => {
                            self.emit_error(
                                "agent scheduler event channel closed unexpectedly".to_string(),
                            )
                            .await;
                            break;
                        }
                    }
                }
            }
        }

        self.terminal.shutdown();
        let _ = self.session.try_send(SessionCommand::Shutdown);
        let _ = self
            .agent_scheduler
            .try_send(AgentSchedulerCommand::Shutdown);
        let _ = self.responses.try_send(ProviderCommand::Shutdown);
    }

    async fn handle_agent_scheduler_event(&mut self, event: AgentSchedulerEvent) {
        match event {
            AgentSchedulerEvent::AgentUpdated(summary) => {
                let _ = self.events.send(RuntimeEvent::AgentUpdated(summary)).await;
            }
            AgentSchedulerEvent::AgentRemoved(agent_id) => {
                let _ = self.events.send(RuntimeEvent::AgentRemoved(agent_id)).await;
            }
            AgentSchedulerEvent::MailboxQueued { agent_id }
            | AgentSchedulerEvent::Interrupted { agent_id } => {
                let _ = self
                    .events
                    .send(RuntimeEvent::AgentMailboxUpdate { agent_id })
                    .await;
            }
            AgentSchedulerEvent::ShutdownComplete => {}
        }
    }

    async fn resume_startup_session(&mut self) {
        let Some(path) = self.config.resume_session_path.clone() else {
            return;
        };
        let event = self
            .session_request(|reply| SessionCommand::Resume { path, reply })
            .await;
        match event {
            Ok(SessionEvent::Resumed { history, path }) => {
                if history.meta.id != self.config.session_id {
                    self.emit_error(format!(
                        "resume session id mismatch: config={} file={}",
                        self.config.session_id, history.meta.id
                    ))
                    .await;
                    return;
                }
                self.active_session = Some(ActiveSession {
                    path,
                    history: history_from_session_records(
                        &history.checkpoint_history,
                        &history.records_after_checkpoint,
                    ),
                    oldest_live_transcript_seq: self.config.initial_transcript_before_seq,
                });
            }
            Ok(SessionEvent::Error { message }) => {
                self.emit_error(message).await;
            }
            Err(error) => {
                self.emit_error(error.to_string()).await;
            }
            Ok(other) => {
                self.emit_error(format!("unexpected session event: {other:?}"))
                    .await;
            }
        }
    }

    async fn handle_input(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if text.starts_with('/') {
            self.handle_slash_command(&text).await;
            return;
        }
        if !matches!(self.root_response_state, RootResponseState::Idle) {
            self.queue_root_steering(text).await;
            return;
        }
        self.submit_input(text).await;
    }

    async fn handle_slash_command(&mut self, text: &str) {
        let mut parts = text.split_whitespace();
        let Some(command_name) = parts.next() else {
            return;
        };
        let Some(command) = self.slash_commands.resolve(command_name) else {
            self.emit_error(format!("unknown slash command `{command_name}`"))
                .await;
            return;
        };
        match *command.action() {
            SlashCommand::Developer => {
                let enabled = match parts.next() {
                    Some("on") => true,
                    Some("off") => false,
                    Some(value) => {
                        self.emit_error(format!(
                            "unsupported developer toggle `{value}`; use on or off"
                        ))
                        .await;
                        return;
                    }
                    None => !self.config.developer_mode,
                };
                if parts.next().is_some() {
                    self.emit_error("usage: /developer [on|off]".to_string())
                        .await;
                    return;
                }
                self.apply_developer_mode(enabled).await;
            }
            SlashCommand::Model => {
                let Some(model) = parts.next() else {
                    self.emit_error("usage: /model <model> [reasoning] [tier]".to_string())
                        .await;
                    return;
                };
                let reasoning_effort = match parts.next() {
                    Some(value) => parse_reasoning_effort(value),
                    None => self.config.reasoning_effort.clone(),
                };
                let service_tier = match parts.next() {
                    Some(value) => match parse_service_tier(value) {
                        Ok(value) => value,
                        Err(error) => {
                            self.emit_error(error.to_string()).await;
                            return;
                        }
                    },
                    None => self.config.service_tier.clone(),
                };
                if parts.next().is_some() {
                    self.emit_error("usage: /model <model> [reasoning] [tier]".to_string())
                        .await;
                    return;
                }
                self.apply_model_settings(ModelSettings::new(
                    model,
                    reasoning_effort,
                    service_tier,
                ))
                .await;
            }
            SlashCommand::Provider => match parts.next() {
                None | Some("show") => {
                    let provider = &self.config.provider;
                    let runtime = &self.config.provider_runtime;
                    let base_url = runtime.profile.driver.base_url();
                    let transport = runtime.profile.driver.transport().label();
                    let _ = self
                        .events
                        .send(RuntimeEvent::TranscriptLine(format!(
                            "provider: {} kind={} transport={} base_url={} models={}",
                            provider.display_name,
                            provider.kind.label(),
                            transport,
                            base_url,
                            runtime.model_catalog.models.len()
                        )))
                        .await;
                }
                Some("list") => {
                    let Some(store) = self.config.provider_store.as_ref() else {
                        self.emit_error("no provider store configured".to_string())
                            .await;
                        return;
                    };
                    let config = match store.load() {
                        Ok(Some(config)) => config,
                        Ok(None) => {
                            self.emit_error("no provider profiles configured".to_string())
                                .await;
                            return;
                        }
                        Err(error) => {
                            self.emit_error(format!("failed to load provider profiles: {error}"))
                                .await;
                            return;
                        }
                    };
                    let listing = format_provider_profile_list(
                        &config,
                        &self.config.provider_runtime.profile.id,
                    );
                    let _ = self
                        .events
                        .send(RuntimeEvent::TranscriptLine(listing))
                        .await;
                }
                Some("use") => {
                    let Some(id) = parts.next() else {
                        self.emit_error("usage: /provider use <profile>".to_string())
                            .await;
                        return;
                    };
                    if parts.next().is_some() {
                        self.emit_error("usage: /provider use <profile>".to_string())
                            .await;
                        return;
                    }
                    self.switch_provider_profile(&ProviderProfileId::new(id))
                        .await;
                }
                Some("test") => {
                    let Some(id) = parts.next() else {
                        self.emit_error("usage: /provider test <profile> [basic|full]".to_string())
                            .await;
                        return;
                    };
                    let mode = match parts.next() {
                        None | Some("basic") => ProviderTestMode::Basic,
                        Some("full") => ProviderTestMode::Full,
                        Some(mode) => {
                            self.emit_error(format!(
                                "unknown test mode `{mode}`; use basic or full"
                            ))
                            .await;
                            return;
                        }
                    };
                    if parts.next().is_some() {
                        self.emit_error("usage: /provider test <profile> [basic|full]".to_string())
                            .await;
                        return;
                    }
                    self.test_provider_profile(&ProviderProfileId::new(id), mode)
                        .await;
                }
                Some(sub) => {
                    self.emit_error(format!(
                        "unknown /provider subcommand `{sub}`; use show, list, use, or test"
                    ))
                    .await;
                }
            },
            SlashCommand::ToolsOverride => match parts.next() {
                None | Some("show") => {
                    self.emit_tools_override_status().await;
                }
                Some("custom") => {
                    if parts.next().is_some() {
                        self.emit_error("usage: /toolsoverride [custom|compat]".to_string())
                            .await;
                        return;
                    }
                    self.apply_tools_override(ToolsOverrideMode::Custom).await;
                }
                Some("compat") => {
                    if parts.next().is_some() {
                        self.emit_error("usage: /toolsoverride [custom|compat]".to_string())
                            .await;
                        return;
                    }
                    self.apply_tools_override(ToolsOverrideMode::Compat).await;
                }
                Some(mode) => {
                    self.emit_error(format!(
                        "unknown tools override `{mode}`; use custom or compat"
                    ))
                    .await;
                }
            },
            SlashCommand::Terminal => match parts.next() {
                None | Some("show") => {
                    self.emit_terminal_tools_status().await;
                }
                Some("on") => {
                    if parts.next().is_some() {
                        self.emit_error("usage: /terminal [on|off]".to_string())
                            .await;
                        return;
                    }
                    self.apply_terminal_tools(true).await;
                }
                Some("off") => {
                    if parts.next().is_some() {
                        self.emit_error("usage: /terminal [on|off]".to_string())
                            .await;
                        return;
                    }
                    self.apply_terminal_tools(false).await;
                }
                Some(mode) => {
                    self.emit_error(format!("unknown terminal setting `{mode}`; use on or off"))
                        .await;
                }
            },
            SlashCommand::Compact => {
                let prompt = parts.collect::<Vec<_>>().join(" ");
                self.compact_current_session(
                    manual_compaction_prompt(&prompt),
                    CompactionTrigger::ContextWindowPressure,
                )
                .await;
            }
            SlashCommand::Rollback => {
                let Some(seq_text) = parts.next() else {
                    self.emit_error("usage: /rollback <seq>".to_string()).await;
                    return;
                };
                if parts.next().is_some() {
                    self.emit_error("usage: /rollback <seq>".to_string()).await;
                    return;
                }
                let Ok(seq) = seq_text.parse::<u64>() else {
                    self.emit_error(format!(
                        "invalid rollback sequence `{seq_text}`; expected an unsigned integer"
                    ))
                    .await;
                    return;
                };
                self.rollback_session(seq).await;
            }
            SlashCommand::Persist => {
                let task = parts.collect::<Vec<_>>().join(" ");
                self.apply_persist_command(task).await;
            }
        }
    }

    async fn apply_developer_mode(&mut self, enabled: bool) {
        self.config.developer_mode = enabled;
        let _ = self
            .events
            .send(RuntimeEvent::DeveloperModeChanged(enabled))
            .await;
        let role = if enabled { "developer" } else { "user" };
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!("input role: {role}")))
            .await;
    }

    async fn emit_tools_override_status(&mut self) {
        let mode = tools_override_mode_for_registry(&self.config.tool_registry);
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!(
                "tools override: {}",
                mode.status_label()
            )))
            .await;
    }

    async fn apply_tools_override(&mut self, mode: ToolsOverrideMode) {
        self.config.tool_registry = mode.tool_registry();
        if !self.config.terminal_tools_enabled {
            self.config.tool_registry = self.config.tool_registry.without_terminal_tools();
        }
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!(
                "tools override: {}",
                mode.status_label()
            )))
            .await;
    }

    async fn emit_terminal_tools_status(&mut self) {
        let state = if self.config.terminal_tools_enabled {
            "on"
        } else {
            "off"
        };
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!(
                "terminal tools: {state}"
            )))
            .await;
    }

    /// Rebuild the tool registry from the active provider, honoring the
    /// terminal-tools toggle, and announce the new state.
    async fn apply_terminal_tools(&mut self, enabled: bool) {
        self.config.terminal_tools_enabled = enabled;
        let base = crate::provider_runtime::tool_registry_for_provider(
            self.config.provider_runtime.profile.kind,
        );
        self.config.tool_registry = if enabled {
            base
        } else {
            base.without_terminal_tools()
        };
        let state = if enabled { "on" } else { "off" };
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!(
                "terminal tools: {state}"
            )))
            .await;
    }

    async fn apply_model_settings(&mut self, settings: ModelSettings) {
        let normalized = normalize_model_slug(&settings.model);
        let Some(entry) = self
            .config
            .provider_runtime
            .model_catalog
            .entry(&normalized)
        else {
            self.emit_error(format!(
                "model `{}` is not available from provider `{}`",
                settings.model, self.config.provider_runtime.profile.display_name
            ))
            .await;
            return;
        };
        let context_window_policy = match context_window_policy_from_model_info(&entry.info) {
            Ok(policy) => policy,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                return;
            }
        };

        let new_runtime =
            match self
                .config
                .provider_runtime
                .with_model_settings(ModelSettings::new(
                    normalized,
                    settings.reasoning_effort.clone(),
                    settings.service_tier.clone(),
                )) {
                Ok(runtime) => runtime,
                Err(error) => {
                    self.emit_error(error.to_string()).await;
                    return;
                }
            };

        self.config.provider_runtime = new_runtime.clone();
        self.config.model = new_runtime.selected_model.model.clone();
        self.config.reasoning_effort = new_runtime.selected_model.reasoning_effort.clone();
        self.config.service_tier = new_runtime.selected_model.service_tier.clone();
        self.config.model_capabilities = new_runtime.selected_capabilities;
        self.config.model_catalog = new_runtime.model_catalog.raw_models();
        self.config.context_window_policy = Some(context_window_policy);

        let _ = self
            .events
            .send(RuntimeEvent::ModelSettingsChanged(
                new_runtime.selected_model.clone(),
            ))
            .await;
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!(
                "model: {} reasoning={} tier={}",
                new_runtime.selected_model.model,
                new_runtime.selected_model.display_reasoning_effort(),
                new_runtime.selected_model.display_service_tier()
            )))
            .await;
        self.emit_current_context_window_usage().await;
    }

    async fn switch_provider_profile(&mut self, id: &ProviderProfileId) {
        let Some(store) = self.config.provider_store.as_ref() else {
            self.emit_error("no provider store configured".to_string())
                .await;
            return;
        };

        let mut config = match store.load() {
            Ok(Some(config)) => config,
            Ok(None) => {
                self.emit_error("no provider profiles configured".to_string())
                    .await;
                return;
            }
            Err(error) => {
                self.emit_error(format!("failed to load provider profiles: {error}"))
                    .await;
                return;
            }
        };

        let Some(profile) = config.profile(id).cloned() else {
            self.emit_error(format!("provider profile `{id}` not found"))
                .await;
            return;
        };
        if self.root_agentic_loop_active {
            self.pending_provider_profile_id = Some(id.clone());
            let _ = self
                .events
                .send(RuntimeEvent::TranscriptLine(format!(
                    "provider switch queued: {id}"
                )))
                .await;
            return;
        }

        let mut builder = ProviderRuntimeBuilder::new(profile.clone()).with_codex_home(
            self.config.codex_home.clone().unwrap_or_else(|| {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
                    .join(".codex")
            }),
        );
        if let Some(store) = self.config.credential_store.as_ref() {
            builder = builder.with_credential_store(store.clone());
        }

        let current_settings = self.config.provider_runtime.selected_model.clone();
        let runtime_result = builder
            .clone()
            .build_async_with_model(Some(current_settings.clone()))
            .await;
        let new_runtime = match runtime_result {
            Ok(runtime) => runtime,
            Err(ProviderError::MissingModel { .. }) => {
                let default_settings = ModelSettings::new(
                    profile.default_model.clone(),
                    profile.default_reasoning_effort.clone(),
                    profile.default_service_tier.clone(),
                );
                match builder.build_async_with_model(Some(default_settings)).await {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        self.emit_error(format!(
                            "failed to switch to provider `{id}` with default model: {error}"
                        ))
                        .await;
                        return;
                    }
                }
            }
            Err(error) => {
                self.emit_error(format!("failed to switch to provider `{id}`: {error}"))
                    .await;
                return;
            }
        };

        config.default_profile_id = id.clone();
        if let Err(error) = store.save(&config) {
            self.emit_error(format!(
                "failed to save provider profile selection: {error}"
            ))
            .await;
            return;
        }

        let _ = self.responses.send(ProviderCommand::Shutdown).await;
        self.responses = provider_transport_for_runtime(&new_runtime);
        // Drop the cached secondary (ollama-cloud) transport so it is rebuilt
        // or released on the next use after a provider switch.
        self.drop_secondary_responses().await;

        let new_analytics = if new_runtime.profile.kind == crate::providers::ProviderKind::Codex {
            match &new_runtime.auth {
                crate::provider_runtime::ProviderAuthRuntime::ChatGpt(_) => {
                    new_runtime.auth.access_token().and_then(|token| {
                        self.config.analytics.as_ref().map(|analytics| {
                            CodexAnalytics::new(
                                new_runtime
                                    .api
                                    .endpoint_url(crate::responses::ApiEndpoint::AnalyticsEvents)
                                    .to_string(),
                                token,
                                analytics.installation_id(),
                            )
                        })
                    })
                }
                crate::provider_runtime::ProviderAuthRuntime::ApiKey(_) => None,
            }
        } else {
            None
        };

        let new_provider_info =
            crate::providers::ProviderUiInfo::from_profile(&new_runtime.profile);
        self.config.provider = new_provider_info.clone();
        self.config.provider_runtime = new_runtime.clone();
        self.config.model = new_runtime.selected_model.model.clone();
        self.config.reasoning_effort = new_runtime.selected_model.reasoning_effort.clone();
        self.config.service_tier = new_runtime.selected_model.service_tier.clone();
        self.config.model_capabilities = new_runtime.selected_capabilities;
        self.config.model_catalog = new_runtime.model_catalog.raw_models();
        self.config.tool_registry =
            crate::provider_runtime::tool_registry_for_provider(new_runtime.profile.kind);
        if !self.config.terminal_tools_enabled {
            self.config.tool_registry = self.config.tool_registry.without_terminal_tools();
        }
        self.config.context_window_policy = Some(new_runtime.context_window_policy);
        self.config.analytics = new_analytics;

        let _ = self
            .events
            .send(RuntimeEvent::ProviderChanged(new_provider_info))
            .await;
        let _ = self
            .events
            .send(RuntimeEvent::ModelSettingsChanged(
                new_runtime.selected_model.clone(),
            ))
            .await;
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!(
                "provider: {} model={} reasoning={} tier={}",
                self.config.provider_runtime.profile.display_name,
                new_runtime.selected_model.model,
                new_runtime.selected_model.display_reasoning_effort(),
                new_runtime.selected_model.display_service_tier()
            )))
            .await;
        self.emit_current_context_window_usage().await;
    }

    async fn test_provider_profile(&mut self, id: &ProviderProfileId, mode: ProviderTestMode) {
        let Some(store) = self.config.provider_store.as_ref() else {
            self.emit_error("no provider store configured".to_string())
                .await;
            return;
        };

        let config = match store.load() {
            Ok(Some(config)) => config,
            Ok(None) => {
                self.emit_error("no provider profiles configured".to_string())
                    .await;
                return;
            }
            Err(error) => {
                self.emit_error(format!("failed to load provider profiles: {error}"))
                    .await;
                return;
            }
        };

        let Some(profile) = config.profile(id).cloned() else {
            self.emit_error(format!("provider profile `{id}` not found"))
                .await;
            return;
        };

        let codex_home = self.config.codex_home.clone().or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| std::path::PathBuf::from(home).join(".codex"))
        });
        let credential_store = self.config.credential_store.clone();

        let report = test_provider_profile(&profile, mode, codex_home, credential_store).await;

        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!(
                "provider test: {} ({} steps)",
                report.profile_id,
                report.steps.len()
            )))
            .await;
        for step in report.steps {
            let details = step
                .details
                .map(|details| format!(" ({details})"))
                .unwrap_or_default();
            let _ = self
                .events
                .send(RuntimeEvent::TranscriptLine(format!(
                    "  [{}] {}{}",
                    step.status, step.label, details
                )))
                .await;
        }
    }

    async fn queue_root_steering(&mut self, text: String) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let queued = append_queued_steering_text(self.root_steering_queue.queued(), text);
        self.root_steering_queue
            .queue_for_next_tool_call(queued.clone());
        let _ = self
            .events
            .send(RuntimeEvent::SteeringQueued(Some(queued)))
            .await;
    }

    async fn apply_root_steering(&mut self, text: String, mode: SteeringMode) {
        let text = self
            .root_steering_queue
            .interrupt_and_take(text.trim().to_string());
        if self.root_persist_active {
            if !text.is_empty() {
                self.root_persist_paused = false;
            } else if mode == SteeringMode::InterruptNow && !self.root_persist_paused {
                self.root_persist_paused = true;
                let _ = self
                    .events
                    .send(RuntimeEvent::TranscriptLine("persist: paused".to_string()))
                    .await;
            }
        }
        let transcript = if text.is_empty() {
            "interrupt requested".to_string()
        } else {
            format!("steering: {text}")
        };
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(transcript))
            .await;
        let _ = self.events.send(RuntimeEvent::SteeringQueued(None)).await;

        self.fire_turn_steer_event_analytics("accepted", None);

        if mode == SteeringMode::InterruptNow {
            self.interrupt_root_response_stream(&text).await;
        }
    }

    fn fire_turn_steer_event_analytics(
        &self,
        result: &'static str,
        rejection_reason: Option<&'static str>,
    ) {
        let Some(analytics) = self.config.analytics.as_ref() else {
            return;
        };
        let active_id = match &self.root_response_state {
            RootResponseState::Streaming(stream) => Some(stream.request_id.0),
            RootResponseState::Submitted { request_id } => Some(request_id.0),
            RootResponseState::Idle => None,
        };
        let expected_turn_id = self.turn_count.max(1);
        analytics.fire_turn_steer_event(crate::analytics::TurnSteerContext {
            session_id: self.config.session_id.as_str().to_string(),
            thread_id: self.config.session_id.as_str().to_string(),
            expected_turn_id,
            accepted_turn_id: active_id,
            num_input_images: 0,
            result,
            rejection_reason,
        });
    }

    async fn apply_queued_root_steering_for_next_tool_call(&mut self) {
        let Some(text) = self.root_steering_queue.take_for_tool_call() else {
            return;
        };
        let transcript = format!("steering: {}", text.trim());
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(transcript))
            .await;
        let _ = self.events.send(RuntimeEvent::SteeringQueued(None)).await;
        self.persist_root_input_message(&root_queued_steering_message(&text))
            .await;
    }
    async fn submit_queued_root_steering_after_response_completion(&mut self) -> bool {
        let Some(_) = self.root_steering_queue.queued() else {
            return false;
        };
        self.apply_queued_root_steering_for_next_tool_call().await;
        self.submit_current_responses_request_or_compact().await;
        true
    }

    async fn interrupt_root_response_stream(&mut self, text: &str) {
        let Some(retired) = self.retire_root_response() else {
            return;
        };
        self.ignored_response_request_ids.insert(retired.request_id);
        self.complete_assistant_message().await;
        self.pending_freeform_tool_inputs.clear();
        self.pending_function_tool_arguments.clear();
        self.emit_root_response_completed(retired.was_streaming)
            .await;

        if text.is_empty() {
            return;
        }
        self.persist_root_steering_message(text).await;
        self.submit_current_responses_request_or_compact().await;
    }

    async fn persist_root_steering_message(&mut self, text: &str) {
        let message = MessageRecord {
            text: text.trim().to_string(),
        };
        if let Some(active_session) = &mut self.active_session {
            active_session
                .history
                .push(HistoryRecord::UserMessage(message.clone()));
        }
        if let Err(error) = self
            .append_session_records(vec![SessionRecordKind::UserMessage(message)], true)
            .await
        {
            self.emit_error(error.to_string()).await;
        }
    }

    async fn apply_persist_command(&mut self, task: String) {
        match persist_command_action(self.root_persist_active, &task) {
            PersistCommandAction::Disable => {
                self.root_persist_active = false;
                self.root_persist_task = None;
                self.root_persist_paused = false;
                let _ = self
                    .events
                    .send(RuntimeEvent::TranscriptLine(
                        "persist: disabled".to_string(),
                    ))
                    .await;
            }
            PersistCommandAction::EnablePreviousTask => {
                self.enable_persist_for_previous_task().await;
            }
            PersistCommandAction::EnableExplicitTask(task) => {
                self.enable_persist_for_task(task).await;
            }
            PersistCommandAction::Continue => {
                if !self.root_persist_active {
                    self.emit_error("persist mode is not active".to_string())
                        .await;
                    return;
                }
                if !self.root_persist_paused {
                    let _ = self
                        .events
                        .send(RuntimeEvent::TranscriptLine(
                            "persist: already active".to_string(),
                        ))
                        .await;
                    return;
                }
                self.root_persist_paused = false;
                let _ = self
                    .events
                    .send(RuntimeEvent::TranscriptLine(
                        "persist: continued".to_string(),
                    ))
                    .await;
                if !self.root_transport_active() {
                    self.persist_root_continuation_prompt().await;
                }
            }
            PersistCommandAction::Pause => {
                if !self.root_persist_active {
                    self.emit_error("persist mode is not active".to_string())
                        .await;
                    return;
                }
                if self.root_persist_paused {
                    let _ = self
                        .events
                        .send(RuntimeEvent::TranscriptLine(
                            "persist: already paused".to_string(),
                        ))
                        .await;
                    return;
                }
                self.root_persist_paused = true;
                let _ = self
                    .events
                    .send(RuntimeEvent::TranscriptLine("persist: paused".to_string()))
                    .await;
            }
        }
    }

    async fn enable_persist_for_previous_task(&mut self) {
        let Some(task) = self.previous_persist_task() else {
            self.emit_error("no previous task to persist".to_string())
                .await;
            return;
        };
        self.root_persist_task = Some(task);
        self.enable_persist_mode().await;
    }

    async fn enable_persist_for_task(&mut self, task: String) {
        if self.persist_root_user_task(task.clone()).await {
            self.root_persist_task = Some(task);
            self.enable_persist_mode().await;
        }
    }

    fn previous_persist_task(&self) -> Option<String> {
        self.active_session
            .as_ref()?
            .history
            .iter()
            .rev()
            .find_map(|record| match record {
                HistoryRecord::UserMessage(message) | HistoryRecord::DeveloperMessage(message) => {
                    Some(message.text.clone())
                }
                _ => None,
            })
    }

    async fn enable_persist_mode(&mut self) {
        self.root_persist_active = true;
        self.root_persist_paused = false;
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine("persist: enabled".to_string()))
            .await;

        if self.root_transport_active() {
            return;
        }
        self.persist_root_continuation_prompt().await;
    }

    async fn persist_root_continuation_prompt(&mut self) {
        if !self.root_persist_active {
            return;
        }
        let task = self
            .root_persist_task
            .as_deref()
            .unwrap_or("the current task");
        let message = persist_continuation_message(task);
        self.persist_root_input_message(&message).await;
        self.submit_current_responses_request_or_compact().await;
    }

    async fn persist_root_user_task(&mut self, task: String) -> bool {
        self.retire_root_response_for_continuation().await;
        self.complete_assistant_message().await;
        if let Err(error) = self.ensure_session(&task).await {
            self.emit_error(error.to_string()).await;
            return false;
        }

        let turn_context = SessionRecordKind::TurnContext(TurnContextRecord {
            cwd: self.config.cwd.clone(),
            date: utc_date_from_unix_ms(now_unix_ms()),
            timezone: "UTC".to_string(),
            model: self.config.model.clone(),
            sandbox_policy: self.config.sandbox_policy.clone(),
            approval_policy: self.config.approval_policy.clone(),
        });
        let message_record = MessageRecord { text: task.clone() };
        let message_kind = SessionRecordKind::UserMessage(message_record.clone());
        let seqs = match self
            .append_session_records(vec![turn_context, message_kind], true)
            .await
        {
            Ok(seqs) => seqs,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                return false;
            }
        };
        if let Some(seq) = seqs.get(1).copied() {
            self.note_live_transcript_seq(seq);
        }

        if let Some(active_session) = &mut self.active_session {
            active_session
                .history
                .push(HistoryRecord::UserMessage(message_record));
        }
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!("user> {task}")))
            .await;
        true
    }

    async fn persist_root_input_message(&mut self, text: &str) {
        let message = MessageRecord {
            text: text.to_string(),
        };
        let input_role = self.configured_input_role();
        if let Some(active_session) = &mut self.active_session {
            active_session
                .history
                .push(input_role.history_record(message.clone()));
        }
        match self
            .append_session_records(vec![input_role.session_record_kind(message)], true)
            .await
        {
            Ok(seqs) => {
                if let Some(seq) = seqs.first().copied() {
                    self.note_live_transcript_seq(seq);
                }
            }
            Err(error) => self.emit_error(error.to_string()).await,
        }
    }

    async fn set_root_agentic_loop_active(&mut self, active: bool) {
        if self.root_agentic_loop_active == active {
            return;
        }
        self.root_agentic_loop_active = active;
        let event = if active {
            RuntimeEvent::AgenticLoopStarted
        } else {
            RuntimeEvent::AgenticLoopCompleted
        };
        let _ = self.events.send(event).await;
    }

    async fn complete_root_agentic_loop_if_idle(&mut self) {
        if self.root_agentic_loop_active && self.root_agentic_loop_idle() {
            self.set_root_agentic_loop_active(false).await;
            self.apply_pending_provider_switch().await;
        }
    }

    async fn apply_pending_provider_switch(&mut self) {
        let Some(id) = self.pending_provider_profile_id.take() else {
            return;
        };
        self.switch_provider_profile(&id).await;
    }

    fn root_agentic_loop_idle(&self) -> bool {
        !self.root_transport_active()
            && self.pending_root_continuation.is_none()
            && self.active_compaction_groups.is_empty()
            && !self.active_tool_output_summaries.values().any(|summary| {
                matches!(
                    summary.pending,
                    PendingToolOutputSummary::RootFreeform { .. }
                )
            })
    }

    async fn submit_input(&mut self, text: String) {
        if self.root_persist_active && self.root_persist_paused {
            self.root_persist_paused = false;
            let _ = self
                .events
                .send(RuntimeEvent::TranscriptLine(
                    "persist: continued".to_string(),
                ))
                .await;
        } else {
            self.root_persist_active = false;
            self.root_persist_task = None;
            self.root_persist_paused = false;
        }
        self.retire_root_response_for_continuation().await;
        self.complete_assistant_message().await;
        if let Err(error) = self.ensure_session(&text).await {
            self.emit_error(error.to_string()).await;
            return;
        }

        let turn_context = SessionRecordKind::TurnContext(TurnContextRecord {
            cwd: self.config.cwd.clone(),
            date: utc_date_from_unix_ms(now_unix_ms()),
            timezone: "UTC".to_string(),
            model: self.config.model.clone(),
            sandbox_policy: self.config.sandbox_policy.clone(),
            approval_policy: self.config.approval_policy.clone(),
        });
        let input_role = self.configured_input_role();
        let message_record = MessageRecord { text: text.clone() };
        let message_kind = input_role.session_record_kind(message_record.clone());
        let seqs = match self
            .append_session_records(vec![turn_context, message_kind], true)
            .await
        {
            Ok(seqs) => seqs,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                return;
            }
        };
        if let Some(seq) = seqs.get(1).copied() {
            self.note_live_transcript_seq(seq);
        }

        if let Some(active_session) = &mut self.active_session {
            active_session
                .history
                .push(input_role.history_record(message_record));
        }
        let transcript_prefix = input_role.transcript_prefix();
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!(
                "{transcript_prefix} {text}"
            )))
            .await;

        self.turn_count += 1;
        self.submit_current_responses_request_or_compact().await;
    }

    async fn submit_responses_request(&mut self, request: ResponsesRequest) {
        self.retire_root_response_for_continuation().await;
        let request_id = request.request_id;
        let result = self
            .responses
            .send(ProviderCommand::Submit {
                request,
                events: self.response_events_tx.clone(),
            })
            .await;
        match result {
            Ok(()) => {
                self.root_response_state = RootResponseState::Submitted { request_id };
                self.set_root_agentic_loop_active(true).await;
            }
            Err(err) => {
                let _ = self
                    .events
                    .send(RuntimeEvent::TranscriptLine(format!(
                        "responses actor error: {err}"
                    )))
                    .await;
                self.complete_root_agentic_loop_if_idle().await;
            }
        }
    }

    async fn load_transcript_page(&mut self, before_seq: Option<u64>, max_lines: usize) {
        let Some(active_session) = self.active_session.as_ref() else {
            let _ = self
                .events
                .send(RuntimeEvent::TranscriptPage(TranscriptPage {
                    lines: Vec::new(),
                    next_before_seq: None,
                    reached_start: true,
                }))
                .await;
            return;
        };
        let path = active_session.path.clone();
        let before_seq = before_seq.or(active_session.oldest_live_transcript_seq);
        let event = self
            .session_request(|reply| SessionCommand::LoadTranscriptPage {
                path,
                before_seq,
                max_lines,
                reply,
            })
            .await;
        match event {
            Ok(SessionEvent::TranscriptPageLoaded { page }) => {
                let _ = self.events.send(RuntimeEvent::TranscriptPage(page)).await;
            }
            Ok(SessionEvent::Error { message }) => {
                self.emit_error(message).await;
            }
            Err(error) => {
                self.emit_error(error.to_string()).await;
            }
            Ok(other) => {
                self.emit_error(format!("unexpected session event: {other:?}"))
                    .await;
            }
        }
    }

    async fn close_idle_responses_connections(&mut self) {
        let _ = self.responses.send(ProviderCommand::CloseIdle).await;
    }

    async fn retry_current_root_response_after_reconnect(
        &mut self,
        request_id: ResponsesRequestId,
    ) {
        self.clear_root_response_state(request_id).await;
        self.pending_assistant_text.clear();
        self.pending_root_continuation = None;
        self.close_idle_responses_connections().await;
        let request = match self.build_responses_request() {
            Ok(request) => request,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                self.complete_root_agentic_loop_if_idle().await;
                return;
            }
        };
        self.submit_responses_request(request).await;
    }

    async fn emit_current_context_window_usage(&mut self) {
        let Some(policy) = self.config.context_window_policy else {
            return;
        };
        let Ok(request) = self.current_compact_request(auto_compaction_prompt(None)) else {
            return;
        };
        let usage = policy.assess_request(&request).usage();
        let _ = self
            .events
            .send(RuntimeEvent::ContextWindowUsage(usage))
            .await;
    }

    async fn submit_current_responses_request_or_compact(&mut self) {
        if let Some(policy) = self.config.context_window_policy {
            match self.current_compact_request(auto_compaction_prompt(None)) {
                Ok(request) => {
                    let decision = policy.assess_request(&request);
                    let usage = decision.usage();
                    let _ = self
                        .events
                        .send(RuntimeEvent::ContextWindowUsage(usage))
                        .await;
                    if let ContextWindowDecision::Compact(usage) = decision {
                        let Some(idle) = self.root_transport_idle() else {
                            self.pending_root_continuation = Some(PendingRootContinuation::Submit);
                            return;
                        };
                        let request = match self
                            .current_compact_request(auto_compaction_prompt(Some(usage)))
                        {
                            Ok(request) => request,
                            Err(error) => {
                                self.emit_error(error.to_string()).await;
                                self.complete_root_agentic_loop_if_idle().await;
                                return;
                            }
                        };
                        self.start_compaction(
                            idle,
                            CompactionTrigger::ContextWindowPressure,
                            request,
                        )
                        .await;
                        return;
                    }
                }
                Err(error) => {
                    self.emit_error(error.to_string()).await;
                    self.complete_root_agentic_loop_if_idle().await;
                    return;
                }
            }
        }

        let request = match self.build_responses_request() {
            Ok(request) => request,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                self.complete_root_agentic_loop_if_idle().await;
                return;
            }
        };
        self.submit_responses_request(request).await;
    }

    async fn compact_current_session(&mut self, prompt: String, trigger: CompactionTrigger) {
        self.compact_current_session_with_trigger(prompt, trigger)
            .await;
    }

    async fn compact_current_session_with_trigger(
        &mut self,
        prompt: String,
        trigger: CompactionTrigger,
    ) {
        let Some(idle) = self.root_transport_idle() else {
            self.pending_root_continuation =
                Some(PendingRootContinuation::CompactCurrentSession { prompt, trigger });
            return;
        };

        self.complete_assistant_message().await;
        let request = match self.current_compact_request(prompt) {
            Ok(request) => request,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                self.complete_root_agentic_loop_if_idle().await;
                return;
            }
        };
        self.start_compaction(idle, trigger, request).await;
    }

    async fn compact_with_request(&mut self, trigger: CompactionTrigger, request: CompactRequest) {
        self.compact_with_request_attempt(trigger, request, 1).await;
    }

    async fn compact_with_request_attempt(
        &mut self,
        trigger: CompactionTrigger,
        request: CompactRequest,
        attempt: u8,
    ) {
        let Some(idle) = self.root_transport_idle() else {
            self.pending_root_continuation = Some(PendingRootContinuation::CompactRequest {
                trigger,
                request,
                attempt,
            });
            return;
        };

        self.complete_assistant_message().await;
        self.start_compaction_with_attempt(idle, trigger, request, attempt)
            .await;
    }

    fn current_compact_request(
        &self,
        prompt: String,
    ) -> Result<CompactRequest, CompactionRequestError> {
        let Some(active_session) = self.active_session.as_ref() else {
            return Err(CompactionRequestError::NoActiveSession);
        };
        if active_session.history.is_empty() {
            return Err(CompactionRequestError::EmptyHistory);
        }
        let tool_registry = self.root_tool_registry();
        let history_items_json =
            history_to_responses_input(&active_session.history, &tool_registry)?;
        Ok(CompactRequest::new(prompt, history_items_json))
    }

    async fn start_compaction(
        &mut self,
        idle: RootTransportIdle,
        trigger: CompactionTrigger,
        request: CompactRequest,
    ) {
        self.start_compaction_with_attempt(idle, trigger, request, 1)
            .await;
    }

    async fn start_compaction_with_attempt(
        &mut self,
        _idle: RootTransportIdle,
        trigger: CompactionTrigger,
        request: CompactRequest,
        attempt: u8,
    ) {
        if self.active_session.is_none() {
            self.emit_error("no active session to compact".to_string())
                .await;
            self.complete_root_agentic_loop_if_idle().await;
            return;
        }
        if self.active_compaction_groups.is_empty() {
            if attempt == 1 {
                let _ = self
                    .events
                    .send(RuntimeEvent::TranscriptLine(format!(
                        "compact requested: {}",
                        request.prompt
                    )))
                    .await;
            }
        } else {
            self.emit_error("compaction is already running".to_string())
                .await;
            self.complete_root_agentic_loop_if_idle().await;
            return;
        }

        let plan = match plan_compaction(trigger, request.clone()) {
            Ok(plan) => plan,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                self.complete_root_agentic_loop_if_idle().await;
                return;
            }
        };
        let group_id = CompactionGroupId(NEXT_COMPACTION_ID_NONCE.fetch_add(1, Ordering::Relaxed));
        let mut submissions = Vec::new();
        match &plan {
            CompactionPlan::Single(request) => {
                match self.build_compaction_responses_request(request) {
                    Ok(responses_request) => {
                        submissions.push((responses_request, ActiveCompactionBlock::Single))
                    }
                    Err(error) => {
                        self.emit_error(error.to_string()).await;
                        self.complete_root_agentic_loop_if_idle().await;
                        return;
                    }
                }
            }
            CompactionPlan::Parallel(plan) => {
                for (request, block) in [
                    (&plan.older, ActiveCompactionBlock::Older),
                    (&plan.newer, ActiveCompactionBlock::Newer),
                ] {
                    match self.build_compaction_responses_request(request) {
                        Ok(responses_request) => submissions.push((responses_request, block)),
                        Err(error) => {
                            self.emit_error(error.to_string()).await;
                            self.complete_root_agentic_loop_if_idle().await;
                            return;
                        }
                    }
                }
            }
        }

        self.set_root_agentic_loop_active(true).await;
        self.active_compaction_groups.insert(
            group_id,
            ActiveCompactionGroup {
                trigger,
                original_request: request,
                plan,
                older_result: None,
                newer_result: None,
                attempt,
            },
        );

        let mut sent_request_ids = Vec::new();
        for (responses_request, block) in submissions {
            let request_id = responses_request.request_id;
            self.active_compaction_streams.insert(
                request_id,
                ActiveCompactionStream {
                    group_id,
                    block,
                    pending_text: String::new(),
                },
            );
            let result = self
                .responses
                .send(ProviderCommand::Submit {
                    request: responses_request,
                    events: self.response_events_tx.clone(),
                })
                .await;
            if let Err(err) = result {
                self.active_compaction_groups.remove(&group_id);
                for request_id in sent_request_ids {
                    self.ignored_compaction_request_ids.insert(request_id);
                    self.active_compaction_streams.remove(&request_id);
                }
                self.active_compaction_streams.remove(&request_id);
                self.emit_error(format!("responses actor error: {err}"))
                    .await;
                self.complete_root_agentic_loop_if_idle().await;
                return;
            }
            sent_request_ids.push(request_id);
        }
    }

    fn build_compaction_responses_request(
        &self,
        request: &CompactRequest,
    ) -> Result<ResponsesRequest, CompactionRequestError> {
        let mut input = request.history_items_json.clone();
        input.push(
            message_input(self.configured_input_role(), &request.prompt)
                .map_err(CompactionRequestError::PromptMessage)?,
        );
        let body =
            ResponsesCreateRequest::new(self.config.model.clone(), self.config.model_capabilities)
                .with_instructions(compaction_instructions(&self.config.instructions))
                .with_input(input)
                .with_tools(Vec::new())
                .with_model_settings(&ModelSettings::new(
                    self.config.model.clone(),
                    self.config.reasoning_effort.clone(),
                    self.config.service_tier.clone(),
                ))
                .to_body()?;
        Ok(ResponsesRequest {
            request_id: ResponsesRequestId::new(),
            headers: self.config.responses_headers.clone(),
            body,
        })
    }

    async fn handle_tool_output_summary_response_event(&mut self, event: &ResponsesEvent) -> bool {
        match event {
            ResponsesEvent::RequestStarted(request_id)
                if self.active_tool_output_summaries.contains_key(request_id) =>
            {
                true
            }
            ResponsesEvent::Frame { request_id, body } => {
                let Some(summary) = self.active_tool_output_summaries.get_mut(request_id) else {
                    return false;
                };
                if let Some(delta) = assistant_text_delta_from_frame(body) {
                    summary.pending_text.push_str(delta);
                }
                true
            }
            ResponsesEvent::RequestCompleted(request_id)
                if self.active_tool_output_summaries.contains_key(request_id) =>
            {
                self.tool_output_summary_completed(*request_id).await;
                true
            }
            ResponsesEvent::Error {
                request_id: Some(request_id),
                message,
            } if self.active_tool_output_summaries.contains_key(request_id) => {
                self.tool_output_summary_failed(*request_id, message.clone())
                    .await;
                true
            }
            ResponsesEvent::FreeformToolCall { request_id, .. }
            | ResponsesEvent::FreeformToolInputDelta { request_id, .. }
            | ResponsesEvent::FunctionToolCall { request_id, .. }
            | ResponsesEvent::FunctionToolInputDelta { request_id, .. }
            | ResponsesEvent::ServerReasoningIncluded { request_id, .. }
            | ResponsesEvent::ModelsEtag { request_id, .. }
            | ResponsesEvent::ServerModel { request_id, .. }
                if self.active_tool_output_summaries.contains_key(request_id) =>
            {
                true
            }
            _ => false,
        }
    }

    async fn handle_compaction_response_event(&mut self, event: &ResponsesEvent) -> bool {
        if let Some(request_id) = response_event_request_id(event)
            && self.ignored_compaction_request_ids.contains(&request_id)
        {
            if matches!(
                event,
                ResponsesEvent::RequestCompleted(_)
                    | ResponsesEvent::Error {
                        request_id: Some(_),
                        ..
                    }
            ) {
                self.ignored_compaction_request_ids.remove(&request_id);
            }
            return true;
        }

        match event {
            ResponsesEvent::RequestStarted(request_id)
                if self.active_compaction_streams.contains_key(request_id) =>
            {
                true
            }
            ResponsesEvent::Frame { request_id, body } => {
                let Some(stream) = self.active_compaction_streams.get_mut(request_id) else {
                    return false;
                };
                if let Some(delta) = assistant_text_delta_from_frame(body) {
                    stream.pending_text.push_str(delta);
                }
                true
            }
            ResponsesEvent::RequestCompleted(request_id)
                if self.active_compaction_streams.contains_key(request_id) =>
            {
                self.compaction_request_completed(*request_id).await;
                true
            }
            ResponsesEvent::Error {
                request_id: Some(request_id),
                message,
            } if self.active_compaction_streams.contains_key(request_id) => {
                self.compaction_request_failed(*request_id, message.clone())
                    .await;
                true
            }
            ResponsesEvent::FreeformToolCall { request_id, .. }
            | ResponsesEvent::FreeformToolInputDelta { request_id, .. }
            | ResponsesEvent::FunctionToolCall { request_id, .. }
            | ResponsesEvent::FunctionToolInputDelta { request_id, .. }
            | ResponsesEvent::ServerReasoningIncluded { request_id, .. }
            | ResponsesEvent::ModelsEtag { request_id, .. }
            | ResponsesEvent::ServerModel { request_id, .. }
                if self.active_compaction_streams.contains_key(request_id) =>
            {
                true
            }
            _ => false,
        }
    }

    async fn compaction_request_completed(&mut self, request_id: ResponsesRequestId) {
        let Some(stream) = self.active_compaction_streams.remove(&request_id) else {
            return;
        };
        let result = compact_result_from_text(stream.pending_text);
        let mut completed_result = None;
        if let Some(group) = self.active_compaction_groups.get_mut(&stream.group_id) {
            match stream.block {
                ActiveCompactionBlock::Single => {
                    completed_result = Some(result);
                }
                ActiveCompactionBlock::Older => {
                    group.older_result = Some(result);
                    if let (Some(older), Some(newer)) =
                        (group.older_result.clone(), group.newer_result.clone())
                    {
                        if let CompactionPlan::Parallel(plan) = &group.plan {
                            completed_result = Some(plan.merge_results(older, newer));
                        }
                    }
                }
                ActiveCompactionBlock::Newer => {
                    group.newer_result = Some(result);
                    if let (Some(older), Some(newer)) =
                        (group.older_result.clone(), group.newer_result.clone())
                    {
                        if let CompactionPlan::Parallel(plan) = &group.plan {
                            completed_result = Some(plan.merge_results(older, newer));
                        }
                    }
                }
            }
        }

        if let Some(result) = completed_result {
            if self
                .active_compaction_groups
                .remove(&stream.group_id)
                .is_none()
            {
                return;
            }
            self.commit_compaction_result(result).await;
        }
    }

    async fn compaction_request_failed(&mut self, request_id: ResponsesRequestId, message: String) {
        let Some(stream) = self.active_compaction_streams.remove(&request_id) else {
            return;
        };
        let Some(group) = self.active_compaction_groups.remove(&stream.group_id) else {
            self.emit_error(message.clone()).await;
            if is_reconnect_required_response_error(&message) {
                self.close_idle_responses_connections().await;
            }
            self.complete_root_agentic_loop_if_idle().await;
            return;
        };
        self.remove_compaction_group_streams(stream.group_id);
        if is_context_window_error(&message)
            && group.trigger != CompactionTrigger::ContextWindowError
        {
            let Some(idle) = self.root_transport_idle() else {
                self.pending_root_continuation = Some(PendingRootContinuation::CompactRequest {
                    trigger: CompactionTrigger::ContextWindowError,
                    request: group.original_request,
                    attempt: 1,
                });
                return;
            };
            self.start_compaction(
                idle,
                CompactionTrigger::ContextWindowError,
                group.original_request,
            )
            .await;
        } else if is_reconnect_required_response_error(&message) {
            self.close_idle_responses_connections().await;
            self.emit_error(message.clone()).await;
            self.retry_compaction(group).await;
        } else {
            self.emit_error(message).await;
            self.complete_root_agentic_loop_if_idle().await;
        }
    }

    async fn retry_compaction(&mut self, mut group: ActiveCompactionGroup) {
        group.attempt += 1;
        let Some(idle) = self.root_transport_idle() else {
            self.pending_root_continuation = Some(PendingRootContinuation::CompactRequest {
                trigger: group.trigger,
                request: group.original_request,
                attempt: group.attempt,
            });
            return;
        };
        let request = group.original_request;
        self.start_compaction_with_attempt(idle, group.trigger, request, group.attempt)
            .await;
    }

    fn remove_compaction_group_streams(&mut self, group_id: CompactionGroupId) {
        let request_ids = self
            .active_compaction_streams
            .iter()
            .filter(|(_, stream)| stream.group_id == group_id)
            .map(|(request_id, _)| *request_id)
            .collect::<Vec<_>>();
        for request_id in request_ids {
            self.active_compaction_streams.remove(&request_id);
            self.ignored_compaction_request_ids.insert(request_id);
        }
    }

    async fn commit_compaction_result(&mut self, result: CompactResult) {
        let summary = normalized_compaction_summary(&result.summary);
        let replacement_history =
            vec![self.configured_input_role().history_record(MessageRecord {
                text: compaction_checkpoint_text(&summary),
            })];
        let record = SessionRecordKind::CompactionCheckpoint(CompactionCheckpointRecord {
            replacement_history: replacement_history.clone(),
        });
        if let Err(error) = self.append_session_records(vec![record], true).await {
            self.emit_error(error.to_string()).await;
            self.complete_root_agentic_loop_if_idle().await;
            return;
        }
        if let Some(active_session) = &mut self.active_session {
            active_session.history = replacement_history.clone();
        }
        let tool_registry = self.root_tool_registry();
        let replacement_history_json =
            match history_to_responses_input(&replacement_history, &tool_registry) {
                Ok(replacement_history_json) => replacement_history_json,
                Err(error) => {
                    self.emit_error(error.to_string()).await;
                    self.complete_root_agentic_loop_if_idle().await;
                    return;
                }
            };
        let completed = CompactResult::new(summary, replacement_history_json);
        let _ = self
            .events
            .send(RuntimeEvent::CompactCompleted(completed))
            .await;
        self.emit_current_context_window_usage().await;

        let request = match self.build_responses_request() {
            Ok(request) => request,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                self.complete_root_agentic_loop_if_idle().await;
                return;
            }
        };
        self.submit_responses_request(request).await;
    }

    async fn maybe_summarize_root_freeform_tool_output(
        &mut self,
        call_record: FreeformToolCallRecord,
        output: NativeToolExecutionOutput,
    ) -> Option<NativeToolExecutionOutput> {
        if !tool_output_needs_summary(&call_record, &output) {
            return Some(output);
        }
        let context = self
            .active_session
            .as_ref()
            .map(|session| recent_tool_summary_context(&session.history))
            .unwrap_or_default();
        let pending = PendingToolOutputSummary::RootFreeform {
            call_record,
            output,
        };
        match self.start_tool_output_summary(context, pending).await {
            Ok(()) => None,
            Err((error, pending)) => {
                let message = error.to_string();
                self.emit_error(message.clone()).await;
                let PendingToolOutputSummary::RootFreeform {
                    call_record,
                    output,
                } = pending;
                let output = tool_output_summary_failure_output(output, &message);
                self.record_freeform_tool_call_output_and_continue(call_record, output)
                    .await;
                None
            }
        }
    }

    async fn start_tool_output_summary(
        &mut self,
        context: String,
        pending: PendingToolOutputSummary,
    ) -> Result<(), (ToolOutputSummaryError, PendingToolOutputSummary)> {
        // Use the ollama-cloud secondary transport when available so summarizer
        // requests don't consume the user's primary provider quota.
        let transport = self
            .secondary_responses()
            .await
            .unwrap_or_else(|| self.responses.clone());
        let settings = self.tool_output_summary_settings();
        let request = match self.build_tool_output_summary_request(&settings, &context, &pending) {
            Ok(request) => request,
            Err(error) => return Err((error, pending)),
        };
        let request_id = request.request_id;
        let (_, call_record, _) = pending_tool_output_summary_parts(&pending, &self.config.cwd);
        let tool_name = call_record.name.clone();
        let truncated_input = call_record
            .input
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let truncated_input = if truncated_input.len() > 60 {
            format!("{}...", &truncated_input[..57])
        } else {
            truncated_input
        };
        let detail = format!("Sent request for '{}' ({})", tool_name, truncated_input);
        let _ = self
            .events
            .send(RuntimeEvent::SubagentActivity {
                activity_id: format!("sum-{}", request_id.0),
                description: "Summarizer".to_string(),
                status: "running".to_string(),
                detail: Some(detail),
            })
            .await;

        self.active_tool_output_summaries.insert(
            request_id,
            ActiveToolOutputSummary {
                pending_text: String::new(),
                context,
                attempt: 1,
                transport: transport.clone(),
                settings,
                pending,
            },
        );
        let result = transport
            .send(ProviderCommand::Submit {
                request,
                events: self.response_events_tx.clone(),
            })
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let summary = self
                    .active_tool_output_summaries
                    .remove(&request_id)
                    .expect("active tool output summary was just inserted");
                let _ = self
                    .events
                    .send(RuntimeEvent::SubagentActivity {
                        activity_id: format!("sum-{}", request_id.0),
                        description: "Summarizer".to_string(),
                        status: "failed".to_string(),
                        detail: Some(format!("Submit error: {error}")),
                    })
                    .await;
                Err((
                    ToolOutputSummaryError::ResponsesActor(error.to_string()),
                    summary.pending,
                ))
            }
        }
    }

    fn build_tool_output_summary_request(
        &self,
        settings: &ModelSettings,
        context: &str,
        pending: &PendingToolOutputSummary,
    ) -> Result<ResponsesRequest, ToolOutputSummaryError> {
        let (cwd, call_record, output) =
            pending_tool_output_summary_parts(pending, &self.config.cwd);
        let prompt = tool_output_summary_prompt(context, cwd, call_record, &output.model_output);
        let input = vec![
            message_input(self.configured_input_role(), &prompt)
                .map_err(ToolOutputSummaryError::PromptMessage)?,
        ];
        let body =
            ResponsesCreateRequest::new(settings.model.clone(), self.config.model_capabilities)
                .with_instructions(tool_output_summary_instructions())
                .with_input(input)
                .with_tools(Vec::new())
                .with_model_settings(settings)
                .to_body()?;
        Ok(ResponsesRequest {
            request_id: ResponsesRequestId::new(),
            headers: self.config.responses_headers.clone(),
            body,
        })
    }

    fn tool_output_summary_settings(&self) -> ModelSettings {
        if let Some(runtime) = &self.ollama_cloud_runtime {
            return ModelSettings::new(
                runtime.profile.tool_output_summary_model.clone(),
                Some("high".to_string()),
                runtime.profile.default_service_tier.clone(),
            );
        }
        ModelSettings::new(
            self.config
                .provider_runtime
                .profile
                .tool_output_summary_model
                .clone(),
            Some("high".to_string()),
            self.config
                .provider_runtime
                .profile
                .default_service_tier
                .clone(),
        )
    }

    async fn tool_output_summary_completed(&mut self, request_id: ResponsesRequestId) {
        let Some(summary) = self.active_tool_output_summaries.remove(&request_id) else {
            return;
        };
        let _ = self
            .events
            .send(RuntimeEvent::SubagentActivity {
                activity_id: format!("sum-{}", request_id.0),
                description: "Summarizer".to_string(),
                status: "completed".to_string(),
                detail: None,
            })
            .await;
        let summary_text = normalized_tool_output_summary(&summary.pending_text);
        self.finish_tool_output_summary(summary.pending, summary_text)
            .await;
    }

    async fn tool_output_summary_failed(
        &mut self,
        request_id: ResponsesRequestId,
        message: String,
    ) {
        let Some(summary) = self.active_tool_output_summaries.remove(&request_id) else {
            return;
        };
        let _ = self
            .events
            .send(RuntimeEvent::SubagentActivity {
                activity_id: format!("sum-{}", request_id.0),
                description: "Summarizer".to_string(),
                status: "failed".to_string(),
                detail: Some(message.clone()),
            })
            .await;
        if is_retryable_response_error(&message)
            && summary.attempt < TOOL_OUTPUT_SUMMARY_MAX_ATTEMPTS
        {
            if is_reconnect_required_response_error(&message) {
                self.close_idle_responses_connections().await;
            }
            self.emit_error(format!("tool output summary failed: {message}"))
                .await;
            self.retry_tool_output_summary(summary).await;
            self.set_root_agentic_loop_active(true).await;
        } else {
            let message = if summary.attempt >= TOOL_OUTPUT_SUMMARY_MAX_ATTEMPTS {
                format!(
                    "{message} after {} tool-output summary attempts",
                    TOOL_OUTPUT_SUMMARY_MAX_ATTEMPTS
                )
            } else {
                message
            };
            self.finish_tool_output_summary_failure(summary.pending, message)
                .await;
            self.complete_root_agentic_loop_if_idle().await;
        }
    }

    async fn retry_tool_output_summary(&mut self, mut summary: ActiveToolOutputSummary) {
        loop {
            summary.pending_text.clear();
            summary.attempt += 1;
            let request = match self.build_tool_output_summary_request(
                &summary.settings,
                &summary.context,
                &summary.pending,
            ) {
                Ok(request) => request,
                Err(error) => {
                    let _ = self
                        .events
                        .send(RuntimeEvent::SubagentActivity {
                            activity_id: format!("sum-retry-build-err"),
                            description: "Summarizer".to_string(),
                            status: "failed".to_string(),
                            detail: Some(format!("Build request error: {error}")),
                        })
                        .await;
                    self.finish_tool_output_summary_failure(summary.pending, error.to_string())
                        .await;
                    self.complete_root_agentic_loop_if_idle().await;
                    return;
                }
            };
            let request_id = request.request_id;
            let (_, call_record, _) =
                pending_tool_output_summary_parts(&summary.pending, &self.config.cwd);
            let tool_name = call_record.name.clone();
            let detail = format!(
                "Retrying request for '{}' (attempt {})",
                tool_name, summary.attempt
            );
            let _ = self
                .events
                .send(RuntimeEvent::SubagentActivity {
                    activity_id: format!("sum-{}", request_id.0),
                    description: "Summarizer".to_string(),
                    status: "running".to_string(),
                    detail: Some(detail),
                })
                .await;
            let transport = summary.transport.clone();
            self.active_tool_output_summaries
                .insert(request_id, summary);
            let result = transport
                .send(ProviderCommand::Submit {
                    request,
                    events: self.response_events_tx.clone(),
                })
                .await;
            match result {
                Ok(()) => return,
                Err(error) => {
                    let Some(removed) = self.active_tool_output_summaries.remove(&request_id)
                    else {
                        return;
                    };
                    let message =
                        format!("responses actor error during tool output summary: {error}");
                    let _ = self
                        .events
                        .send(RuntimeEvent::SubagentActivity {
                            activity_id: format!("sum-{}", request_id.0),
                            description: "Summarizer".to_string(),
                            status: "failed".to_string(),
                            detail: Some(message.clone()),
                        })
                        .await;
                    if removed.attempt >= TOOL_OUTPUT_SUMMARY_MAX_ATTEMPTS {
                        self.finish_tool_output_summary_failure(removed.pending, message)
                            .await;
                        self.complete_root_agentic_loop_if_idle().await;
                        return;
                    }
                    summary = removed;
                }
            }
        }
    }

    async fn finish_tool_output_summary(
        &mut self,
        pending: PendingToolOutputSummary,
        summary_text: String,
    ) {
        match pending {
            PendingToolOutputSummary::RootFreeform {
                call_record,
                output,
            } => {
                let output = summarized_tool_output(output, &summary_text);
                self.record_freeform_tool_call_output_and_continue(call_record, output)
                    .await;
            }
        }
    }

    async fn finish_tool_output_summary_failure(
        &mut self,
        pending: PendingToolOutputSummary,
        message: String,
    ) {
        self.emit_error(format!("tool output summary failed: {message}"))
            .await;
        match pending {
            PendingToolOutputSummary::RootFreeform {
                call_record,
                output,
            } => {
                let output = tool_output_summary_failure_output(output, &message);
                self.record_freeform_tool_call_output_and_continue(call_record, output)
                    .await;
            }
        }
    }

    async fn handle_response_event(&mut self, event: ResponsesEvent) {
        let _ = self
            .events
            .send(RuntimeEvent::Responses(event.clone()))
            .await;
        if self.handle_tool_output_summary_response_event(&event).await {
            return;
        }
        if self.handle_compaction_response_event(&event).await {
            return;
        }
        if self.handle_ignored_root_response_event(&event).await {
            return;
        }
        match event {
            ResponsesEvent::FreeformToolInputDelta { request_id, delta } => {
                if !self.root_response_matches(request_id) {
                    return;
                }
                let key = delta
                    .call_id
                    .clone()
                    .unwrap_or_else(|| delta.item_id.clone());
                self.pending_freeform_tool_inputs
                    .entry(key)
                    .or_default()
                    .push_str(&delta.delta);
                let record =
                    SessionRecordKind::FreeformToolInputDelta(FreeformToolInputDeltaRecord {
                        item_id: delta.item_id,
                        delta: delta.delta,
                    });
                if let Err(error) = self.append_session_records(vec![record], false).await {
                    self.emit_error(error.to_string()).await;
                }
            }
            ResponsesEvent::FreeformToolCall { request_id, call } => {
                if !self.root_response_matches(request_id) {
                    return;
                }
                self.retire_root_response_for_continuation().await;
                let FreeformToolCall {
                    call_id,
                    name,
                    input,
                } = call;
                let input = if input.is_empty() {
                    self.pending_freeform_tool_inputs
                        .remove(&call_id)
                        .unwrap_or(input)
                } else {
                    self.pending_freeform_tool_inputs.remove(&call_id);
                    input
                };
                let call_record = FreeformToolCallRecord {
                    call_id,
                    name,
                    input,
                };
                self.complete_assistant_message().await;
                if call_record.name == MARK_TASK_COMPLETE_TOOL_NAME {
                    if let Some(output) = self.handle_mark_task_complete_call().await {
                        self.record_freeform_tool_call_output_and_continue(call_record, output)
                            .await;
                    }
                    return;
                }
                let _ = self
                    .events
                    .send(RuntimeEvent::FreeformToolCall(call_record.clone()))
                    .await;
                let tool_registry = self.root_tool_registry();
                let handler = tool_registry.handler_for(&call_record.name);
                let output = self
                    .execute_root_freeform_tool_call(handler, call_record.clone())
                    .await;
                if let Some(output) = self
                    .maybe_summarize_root_freeform_tool_output(call_record.clone(), output)
                    .await
                {
                    self.record_freeform_tool_call_output_and_continue(call_record, output)
                        .await;
                }
            }
            ResponsesEvent::FunctionToolInputDelta { request_id, delta } => {
                if !self.root_response_matches(request_id) {
                    return;
                }
                let key = delta.call_id.unwrap_or(delta.item_id);
                self.pending_function_tool_arguments
                    .entry(key)
                    .or_default()
                    .push_str(&delta.delta);
            }
            ResponsesEvent::FunctionToolCall { request_id, call } => {
                if !self.root_response_matches(request_id) {
                    return;
                }
                self.retire_root_response_for_continuation().await;
                self.handle_function_tool_call(call).await;
            }
            ResponsesEvent::RequestStarted(request_id) => {
                self.start_root_response_stream(request_id).await;
            }
            ResponsesEvent::Frame { request_id, body } => {
                if !self.root_response_matches(request_id) {
                    return;
                }
                if let Some(delta) = assistant_text_delta_from_frame(&body) {
                    self.record_assistant_first_token(request_id).await;
                    self.pending_assistant_text.push_str(delta);
                    let _ = self
                        .events
                        .send(RuntimeEvent::AssistantTextDelta(delta.to_string()))
                        .await;
                }
            }
            ResponsesEvent::RequestCompleted(request_id) => {
                if !self.root_response_matches(request_id) {
                    return;
                }
                let duration_ms = self.root_response_duration_ms();
                self.complete_assistant_message().await;
                self.clear_root_response_state(request_id).await;
                self.fire_root_turn_analytics(duration_ms);
                self.fire_root_goal_analytics(duration_ms);
                if let Err(error) = self.flush_session().await {
                    self.emit_error(error.to_string()).await;
                }
                if self
                    .submit_queued_root_steering_after_response_completion()
                    .await
                {
                    return;
                }
                self.run_pending_root_continuation_if_ready().await;
                self.complete_root_agentic_loop_if_idle().await;
            }
            ResponsesEvent::Error {
                request_id,
                message,
            } => {
                let root_error_request_id =
                    request_id.filter(|request_id| self.root_response_matches(*request_id));
                if let Some(request_id) = root_error_request_id
                    && is_context_window_error(&message)
                {
                    self.clear_root_response_state(request_id).await;
                    self.pending_assistant_text.clear();
                    self.pending_root_continuation = None;
                    self.compact_current_session(
                        context_error_compaction_prompt(),
                        CompactionTrigger::ContextWindowError,
                    )
                    .await;
                } else if let Some(request_id) = root_error_request_id
                    && is_reconnect_required_response_error(&message)
                {
                    self.emit_error(message.clone()).await;
                    self.retry_current_root_response_after_reconnect(request_id)
                        .await;
                } else if is_reconnect_required_response_error(&message) {
                    self.emit_error(message.clone()).await;
                    self.close_idle_responses_connections().await;
                    self.complete_root_agentic_loop_if_idle().await;
                } else {
                    self.clear_any_root_response_state().await;
                    self.pending_root_continuation = None;
                    self.emit_error(message).await;
                    self.complete_root_agentic_loop_if_idle().await;
                }
            }
            ResponsesEvent::Prewarmed
            | ResponsesEvent::ServerReasoningIncluded { .. }
            | ResponsesEvent::ModelsEtag { .. }
            | ResponsesEvent::ServerModel { .. }
            | ResponsesEvent::ShutdownComplete => {}
        }
    }

    async fn handle_ignored_root_response_event(&mut self, event: &ResponsesEvent) -> bool {
        let Some(request_id) = response_event_request_id(event) else {
            return false;
        };
        if !self.ignored_response_request_ids.contains(&request_id) {
            return false;
        }
        match event {
            ResponsesEvent::RequestCompleted(_) => {
                self.ignored_response_request_ids.remove(&request_id);
                if self
                    .submit_queued_root_steering_after_response_completion()
                    .await
                {
                    return true;
                }
                self.run_pending_root_continuation_if_ready().await;
                self.complete_root_agentic_loop_if_idle().await;
            }
            ResponsesEvent::Error { message, .. } => {
                self.ignored_response_request_ids.remove(&request_id);
                if is_context_window_error(message) {
                    self.pending_assistant_text.clear();
                    self.pending_root_continuation = None;
                    self.compact_current_session(
                        context_error_compaction_prompt(),
                        CompactionTrigger::ContextWindowError,
                    )
                    .await;
                } else {
                    self.run_pending_root_continuation_if_ready().await;
                    self.complete_root_agentic_loop_if_idle().await;
                }
            }
            _ => {}
        }
        true
    }

    async fn record_freeform_tool_call_output_and_continue(
        &mut self,
        call_record: FreeformToolCallRecord,
        output: NativeToolExecutionOutput,
    ) {
        let output_record = freeform_tool_output_record(call_record.call_id.clone(), output);
        if let Some(active_session) = &mut self.active_session {
            active_session
                .history
                .push(HistoryRecord::FreeformToolCall(call_record.clone()));
            active_session
                .history
                .push(HistoryRecord::FreeformToolOutput(output_record.clone()));
        }
        let records = vec![
            SessionRecordKind::FreeformToolCall(call_record),
            SessionRecordKind::FreeformToolOutput(output_record.clone()),
        ];
        match self.append_session_records(records, true).await {
            Ok(seqs) => {
                if let Some(seq) = seqs.iter().copied().min() {
                    self.note_live_transcript_seq(seq);
                }
            }
            Err(error) => {
                self.emit_error(error.to_string()).await;
                self.complete_root_agentic_loop_if_idle().await;
                return;
            }
        }
        let _ = self
            .events
            .send(RuntimeEvent::FreeformToolOutput(output_record.clone()))
            .await;

        self.apply_queued_root_steering_for_next_tool_call().await;
        self.submit_current_responses_request_or_compact().await;
    }

    async fn handle_function_tool_call(&mut self, call: FunctionToolCall) {
        let arguments = if call.arguments.is_empty() {
            self.pending_function_tool_arguments
                .remove(&call.call_id)
                .unwrap_or(call.arguments)
        } else {
            self.pending_function_tool_arguments.remove(&call.call_id);
            call.arguments
        };
        let call_record = FunctionToolCallRecord {
            call_id: call.call_id,
            name: call.name,
            arguments,
        };
        self.complete_assistant_message().await;

        if let Some(freeform_call_record) = normalized_freeform_call_record(&call_record) {
            let _ = self
                .events
                .send(RuntimeEvent::FreeformToolCall(freeform_call_record.clone()))
                .await;
            let output = execute_function_tool_call(
                self.config.cwd.clone(),
                self.config.tool_registry.handler_for(&call_record.name),
                self.terminal.clone(),
                call_record,
            )
            .await;
            self.record_freeform_tool_call_output_and_continue(freeform_call_record, output)
                .await;
            return;
        }

        let _ = self
            .events
            .send(RuntimeEvent::FunctionToolCall(call_record.clone()))
            .await;

        let output = execute_function_tool_call(
            self.config.cwd.clone(),
            self.config.tool_registry.handler_for(&call_record.name),
            self.terminal.clone(),
            call_record.clone(),
        )
        .await;
        self.record_function_tool_call_output_and_continue(call_record, output)
            .await;
    }

    async fn record_function_tool_call_output_and_continue(
        &mut self,
        call_record: FunctionToolCallRecord,
        output: NativeToolExecutionOutput,
    ) {
        let output_record = function_tool_output_record(call_record.call_id.clone(), output);
        if let Some(active_session) = &mut self.active_session {
            active_session
                .history
                .push(HistoryRecord::FunctionToolCall(call_record.clone()));
            active_session
                .history
                .push(HistoryRecord::FunctionToolOutput(output_record.clone()));
        }
        let records = vec![
            SessionRecordKind::FunctionToolCall(call_record),
            SessionRecordKind::FunctionToolOutput(output_record.clone()),
        ];
        match self.append_session_records(records, true).await {
            Ok(seqs) => {
                if let Some(seq) = seqs.iter().copied().min() {
                    self.note_live_transcript_seq(seq);
                }
            }
            Err(error) => {
                self.emit_error(error.to_string()).await;
                self.complete_root_agentic_loop_if_idle().await;
                return;
            }
        }
        let _ = self
            .events
            .send(RuntimeEvent::FunctionToolOutput(output_record.clone()))
            .await;

        self.apply_queued_root_steering_for_next_tool_call().await;
        self.submit_current_responses_request_or_compact().await;
    }

    async fn record_assistant_first_token(&mut self, request_id: ResponsesRequestId) {
        let RootResponseState::Streaming(stream) = &mut self.root_response_state else {
            return;
        };
        if stream.request_id != request_id || stream.first_token_seen {
            return;
        }
        stream.first_token_seen = true;
        let ttft_ms = stream.started_at.elapsed().as_millis() as u64;
        let _ = self
            .events
            .send(RuntimeEvent::AssistantFirstToken { ttft_ms })
            .await;
    }

    fn root_response_matches(&self, request_id: ResponsesRequestId) -> bool {
        match &self.root_response_state {
            RootResponseState::Idle => false,
            RootResponseState::Submitted {
                request_id: active_request_id,
            }
            | RootResponseState::Streaming(ActiveResponseStream {
                request_id: active_request_id,
                ..
            }) => *active_request_id == request_id,
        }
    }

    fn root_transport_active(&self) -> bool {
        !matches!(self.root_response_state, RootResponseState::Idle)
            || !self.ignored_response_request_ids.is_empty()
    }

    fn root_transport_idle(&self) -> Option<RootTransportIdle> {
        (!self.root_transport_active()).then_some(RootTransportIdle)
    }

    fn root_response_duration_ms(&self) -> u64 {
        match &self.root_response_state {
            RootResponseState::Streaming(stream) => stream.started_at.elapsed().as_millis() as u64,
            _ => 0,
        }
    }

    fn fire_thread_initialized(&self) {
        let Some(analytics) = self.config.analytics.as_ref() else {
            return;
        };
        analytics.fire_thread_initialized(crate::analytics::ThreadInitializedContext {
            session_id: self.config.session_id.as_str().to_string(),
            thread_id: self.config.session_id.as_str().to_string(),
            model: self.config.model.clone(),
            is_first_turn: true,
        });
    }

    fn fire_root_turn_analytics(&self, duration_ms: u64) {
        let Some(analytics) = self.config.analytics.as_ref() else {
            return;
        };
        let service_tier = self
            .config
            .service_tier
            .as_deref()
            .unwrap_or(crate::responses::FAST_SERVICE_TIER);
        let input_items = self
            .active_session
            .as_ref()
            .map(|session| crate::analytics::estimate_history_input_count(&session.history))
            .unwrap_or(0) as i64;
        // Fabricate a boring token estimate: ~250 tokens per input item,
        // ~1500 tokens output per turn, staying comfortably under any
        // reasonable limit.
        let input_tokens = (input_items * 250).max(1000);
        let output_tokens = 1500_i64;
        analytics.fire_turn_event(TurnEventContext {
            session_id: self.config.session_id.as_str().to_string(),
            thread_id: self.config.session_id.as_str().to_string(),
            turn_id: self.turn_count,
            model: self.config.model.clone(),
            service_tier: service_tier.to_string(),
            is_first_turn: self.turn_count <= 1,
            turn_status: "completed",
            input_tokens_estimate: input_tokens,
            output_tokens_estimate: output_tokens,
            duration_ms,
        });
    }

    fn fire_root_goal_analytics(&self, duration_ms: u64) {
        let Some(analytics) = self.config.analytics.as_ref() else {
            return;
        };
        // Fabricate a boring goal accounting trajectory with per-turn noise
        // so the server sees plausible jitter instead of a clean linear ramp.
        let turn_index = self.turn_count.max(1) as i64;
        let token_drift = (self.turn_count.wrapping_mul(2654435761) % 733) as i64 - 360;
        let time_drift = (self.turn_count.wrapping_mul(40503) % 17) as i64 - 8;
        let cumulative_tokens = crate::analytics::FAB_GOAL_INITIAL_TOKENS
            + (turn_index * crate::analytics::FAB_GOAL_TOKEN_INCREMENT)
            + token_drift;
        let cumulative_time_seconds = crate::analytics::FAB_GOAL_INITIAL_TIME_SECS
            + (turn_index * crate::analytics::FAB_GOAL_TIME_INCREMENT_SECS)
            + (duration_ms / 1000) as i64
            + time_drift;
        analytics.fire_goal_usage_accounted(crate::analytics::GoalUsageAccountedContext {
            session_id: self.config.session_id.as_str().to_string(),
            thread_id: self.config.session_id.as_str().to_string(),
            goal_id: format!("goal-{}", self.config.session_id.as_str()),
            turn_id: Some(self.turn_count),
            cumulative_tokens,
            cumulative_time_seconds,
            has_token_budget: crate::analytics::FAB_GOAL_HAS_BUDGET,
            goal_status: "active".to_string(),
        });
    }

    async fn start_root_response_stream(&mut self, request_id: ResponsesRequestId) {
        if !matches!(
            self.root_response_state,
            RootResponseState::Submitted {
                request_id: submitted_request_id
            } if submitted_request_id == request_id
        ) {
            return;
        }
        self.root_response_state = RootResponseState::Streaming(ActiveResponseStream {
            request_id,
            started_at: Instant::now(),
            first_token_seen: false,
        });
        let _ = self.events.send(RuntimeEvent::ResponseStreamStarted).await;
    }

    async fn clear_root_response_state(&mut self, request_id: ResponsesRequestId) {
        if !self.root_response_matches(request_id) {
            return;
        }
        let Some(retired) = self.retire_root_response() else {
            return;
        };
        self.emit_root_response_completed(retired.was_streaming)
            .await;
    }

    async fn clear_any_root_response_state(&mut self) {
        let Some(retired) = self.retire_root_response() else {
            return;
        };
        self.emit_root_response_completed(retired.was_streaming)
            .await;
    }

    async fn retire_root_response_for_continuation(&mut self) {
        let Some(retired) = self.retire_root_response() else {
            return;
        };
        self.ignored_response_request_ids.insert(retired.request_id);
        self.emit_root_response_completed(retired.was_streaming)
            .await;
    }

    async fn run_pending_root_continuation_if_ready(&mut self) {
        if self.root_transport_active() {
            return;
        }
        let Some(continuation) = self.pending_root_continuation.take() else {
            if self.root_persist_active && !self.root_persist_paused {
                self.persist_root_continuation_prompt().await;
            } else {
                self.complete_root_agentic_loop_if_idle().await;
            }
            return;
        };
        match continuation {
            PendingRootContinuation::Submit => {
                self.submit_current_responses_request_or_compact().await;
            }
            PendingRootContinuation::CompactCurrentSession { prompt, trigger } => {
                self.compact_current_session_with_trigger(prompt, trigger)
                    .await;
            }
            PendingRootContinuation::CompactRequest {
                trigger,
                request,
                attempt,
            } => {
                self.compact_with_request_attempt(trigger, request, attempt)
                    .await;
            }
        }
    }

    fn retire_root_response(&mut self) -> Option<RetiredRootResponse> {
        let state = std::mem::replace(&mut self.root_response_state, RootResponseState::Idle);
        match state {
            RootResponseState::Idle => None,
            RootResponseState::Submitted { request_id } => Some(RetiredRootResponse {
                request_id,
                was_streaming: false,
            }),
            RootResponseState::Streaming(stream) => Some(RetiredRootResponse {
                request_id: stream.request_id,
                was_streaming: true,
            }),
        }
    }

    async fn emit_root_response_completed(&mut self, was_streaming: bool) {
        if was_streaming {
            let _ = self
                .events
                .send(RuntimeEvent::ResponseStreamCompleted)
                .await;
        }
    }

    async fn complete_assistant_message(&mut self) {
        let text = std::mem::take(&mut self.pending_assistant_text);
        if text.is_empty() {
            return;
        }
        let message = MessageRecord { text: text.clone() };
        if let Some(active_session) = &mut self.active_session {
            active_session
                .history
                .push(HistoryRecord::AssistantMessage(message.clone()));
        }
        match self
            .append_session_records(vec![SessionRecordKind::AssistantMessage(message)], true)
            .await
        {
            Ok(seqs) => {
                if let Some(seq) = seqs.first().copied() {
                    self.note_live_transcript_seq(seq);
                }
            }
            Err(error) => self.emit_error(error.to_string()).await,
        }
    }

    async fn ensure_session(
        &mut self,
        first_user_message: &str,
    ) -> Result<(), SessionOperationError> {
        if self.active_session.is_some() {
            return Ok(());
        }

        let now_ms = now_unix_ms();
        let meta = SessionMeta {
            id: self.config.session_id.clone(),
            parent_session_id: None,
            forked_from_session_id: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            cwd: self.config.cwd.clone(),
            model: self.config.model.clone(),
            originator: self.config.originator.clone(),
            client_version: self.config.client_version.clone(),
            source: self.config.source.clone(),
            title: Some(title_from_message(first_user_message)),
            preview: Some(preview_from_message(first_user_message)),
        };

        let event = self
            .session_request(|reply| SessionCommand::Create { meta, reply })
            .await?;
        match event {
            SessionEvent::Created { path, .. } => {
                self.active_session = Some(ActiveSession {
                    path,
                    history: Vec::new(),
                    oldest_live_transcript_seq: None,
                });
                self.append_session_records(
                    vec![SessionRecordKind::ProviderSessionBinding(
                        crate::sessions::ProviderSessionBindingRecord {
                            binding: self.config.provider_runtime.session_binding(),
                        },
                    )],
                    true,
                )
                .await?;
                Ok(())
            }
            SessionEvent::Error { message } => Err(SessionOperationError::Actor { message }),
            other => Err(SessionOperationError::Unexpected { event: other }),
        }
    }
    async fn rollback_session(&mut self, seq: u64) {
        if !matches!(self.root_response_state, RootResponseState::Idle) {
            self.emit_error("cannot rollback while a response is active".to_string())
                .await;
            return;
        }
        let Some(active_session) = self.active_session.as_ref() else {
            self.emit_error("no active session to rollback".to_string())
                .await;
            return;
        };
        let source_path = active_session.path.clone();
        let store = crate::sessions::SessionStore::new(self.config.session_root.clone());
        let records = match store.read_records(&source_path) {
            Ok(records) => records,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                return;
            }
        };
        let Some(source_meta) = records.iter().find_map(|record| match &record.kind {
            SessionRecordKind::SessionMeta(meta) => Some(meta.clone()),
            _ => None,
        }) else {
            self.emit_error("session is missing metadata".to_string())
                .await;
            return;
        };
        if !records.iter().any(|record| record.seq == seq) {
            self.emit_error(format!("rollback sequence `{seq}` was not found"))
                .await;
            return;
        }
        let copied_record_kinds = records
            .into_iter()
            .filter(|record| record.seq <= seq)
            .filter_map(|record| match record.kind {
                SessionRecordKind::SessionMeta(_) => None,
                SessionRecordKind::SessionClosed(_) => None,
                SessionRecordKind::ProviderSessionBinding(_) => None,
                kind => Some(kind),
            })
            .collect::<Vec<_>>();
        let now_ms = now_unix_ms();
        let new_session_id = SessionId::new(Uuid::new_v4().to_string());
        let title = copied_record_kinds.iter().find_map(|record| match record {
            SessionRecordKind::UserMessage(message)
            | SessionRecordKind::DeveloperMessage(message) => {
                Some(title_from_message(&message.text))
            }
            _ => None,
        });
        let preview = copied_record_kinds
            .iter()
            .rev()
            .find_map(|record| match record {
                SessionRecordKind::UserMessage(message)
                | SessionRecordKind::DeveloperMessage(message)
                | SessionRecordKind::AssistantMessage(message) => {
                    Some(preview_from_message(&message.text))
                }
                _ => None,
            });
        let meta = SessionMeta {
            id: new_session_id.clone(),
            parent_session_id: Some(source_meta.id.clone()),
            forked_from_session_id: Some(source_meta.id),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            cwd: self.config.cwd.clone(),
            model: self.config.model.clone(),
            originator: self.config.originator.clone(),
            client_version: self.config.client_version.clone(),
            source: self.config.source.clone(),
            title,
            preview,
        };
        let event = match self
            .session_request(|reply| SessionCommand::Create { meta, reply })
            .await
        {
            Ok(event) => event,
            Err(error) => {
                self.emit_error(error.to_string()).await;
                return;
            }
        };
        let SessionEvent::Created { path, .. } = event else {
            match event {
                SessionEvent::Error { message } => {
                    self.emit_error(message).await;
                }
                other => {
                    self.emit_error(format!("unexpected session event: {other:?}"))
                        .await;
                }
            }
            return;
        };
        self.config.session_id = new_session_id;
        self.active_session = Some(ActiveSession {
            path,
            history: Vec::new(),
            oldest_live_transcript_seq: None,
        });
        if let Err(error) = self
            .append_session_records(
                vec![SessionRecordKind::ProviderSessionBinding(
                    crate::sessions::ProviderSessionBindingRecord {
                        binding: self.config.provider_runtime.session_binding(),
                    },
                )],
                true,
            )
            .await
        {
            self.emit_error(error.to_string()).await;
            return;
        }
        if !copied_record_kinds.is_empty()
            && let Err(error) = self
                .append_session_records(copied_record_kinds.clone(), true)
                .await
        {
            self.emit_error(error.to_string()).await;
            return;
        }
        if let Some(active_session) = self.active_session.as_mut() {
            let copied_records = copied_record_kinds
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, kind)| crate::sessions::SessionRecord {
                    seq: index as u64,
                    kind,
                })
                .collect::<Vec<_>>();
            active_session.history = history_from_session_records(&[], &copied_records);
        }
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!(
                "rolled back to seq {seq}; forked session {}",
                self.config.session_id
            )))
            .await;
    }

    /// Emit a SubagentActivity event for the locate worker so it appears in the
    /// inline transcript block just like summarizer and subagent activity.
    async fn emit_locate_activity(
        &mut self,
        activity_id: &str,
        status: &str,
        detail: Option<String>,
    ) {
        let _ = self
            .events
            .send(RuntimeEvent::SubagentActivity {
                activity_id: activity_id.to_string(),
                description: "Locate".to_string(),
                status: status.to_string(),
                detail,
            })
            .await;
    }

    /// Return (or lazily build) the ollama-cloud provider transport.
    ///
    /// Returns `None` when:
    /// - The primary provider is already ollama-cloud (caller should use
    ///   `self.responses` directly), or
    /// - No ollama-cloud profile is available / auth failed.
    ///
    /// Returns a cloned handle so the caller can freely use `self.responses`
    /// as a fallback without borrow conflicts.
    async fn secondary_responses(&mut self) -> Option<ActorHandle<ProviderCommand>> {
        // If the primary provider is ollama-cloud, no secondary needed.
        if self.config.provider_runtime.profile.kind == crate::providers::ProviderKind::OllamaCloud
        {
            return None;
        }

        // Return cached handle (cloned).
        if let Some(handle) = &self.ollama_cloud_responses {
            return Some(handle.clone());
        }

        // Try to build one from the provider store.
        let store = self.config.provider_store.as_ref()?;
        let config = store.load().ok()??;
        let profile = config
            .profiles
            .into_iter()
            .find(|p| p.kind == crate::providers::ProviderKind::OllamaCloud)?;

        let codex_home = self.config.codex_home.clone().unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
                .join(".codex")
        });
        let mut builder = crate::provider_runtime::ProviderRuntimeBuilder::new(profile.clone())
            .with_codex_home(codex_home);
        if let Some(cred_store) = self.config.credential_store.as_ref() {
            builder = builder.with_credential_store(cred_store.clone());
        }
        // Use the profile's default model for summarizer/locate.
        match builder.build_async().await {
            Ok(runtime) => {
                let transport = provider_transport_for_runtime(&runtime);
                self.ollama_cloud_runtime = Some(runtime);
                self.ollama_cloud_responses = Some(transport.clone());
                Some(transport)
            }
            Err(error) => {
                // Not fatal — fall back to primary provider silently.
                eprintln!("warning: failed to build ollama-cloud secondary transport: {error}");
                None
            }
        }
    }

    /// Shut down the cached ollama-cloud secondary transport when the primary
    /// provider switches to ollama-cloud (so we don't leak it).
    async fn drop_secondary_responses(&mut self) {
        self.ollama_cloud_runtime = None;
        if let Some(handle) = self.ollama_cloud_responses.take() {
            let _ = handle.send(ProviderCommand::Shutdown).await;
        }
    }

    fn note_live_transcript_seq(&mut self, seq: u64) {
        let Some(active_session) = self.active_session.as_mut() else {
            return;
        };
        active_session.oldest_live_transcript_seq = Some(
            active_session
                .oldest_live_transcript_seq
                .map_or(seq, |oldest| oldest.min(seq)),
        );
    }

    async fn append_session_records(
        &mut self,
        records: Vec<SessionRecordKind>,
        flush: bool,
    ) -> Result<Vec<u64>, SessionOperationError> {
        let event = self
            .session_request(|reply| SessionCommand::Append {
                records,
                flush,
                reply,
            })
            .await?;
        match event {
            SessionEvent::Appended { seqs } => Ok(seqs),
            SessionEvent::Error { message } => Err(SessionOperationError::Actor { message }),
            other => Err(SessionOperationError::Unexpected { event: other }),
        }
    }

    async fn flush_session(&mut self) -> Result<(), SessionOperationError> {
        let event = self
            .session_request(|reply| SessionCommand::Flush { reply })
            .await?;
        match event {
            SessionEvent::Flushed => Ok(()),
            SessionEvent::Error { message } => Err(SessionOperationError::Actor { message }),
            other => Err(SessionOperationError::Unexpected { event: other }),
        }
    }

    async fn session_request(
        &mut self,
        command: impl FnOnce(ActorSender<SessionEvent>) -> SessionCommand,
    ) -> Result<SessionEvent, ActorRequestError> {
        let (reply_handle, reply_rx) = actor_channel(1);
        self.session
            .send(command(reply_handle.sender()))
            .await
            .map_err(|source| ActorRequestError::Send {
                actor: "session",
                source,
            })?;
        reply_rx
            .recv()
            .await
            .map_err(|source| ActorRequestError::Receive {
                actor: "session",
                source,
            })
    }

    fn build_responses_request(&self) -> Result<ResponsesRequest, RootRequestBuildError> {
        let input = self
            .active_session
            .as_ref()
            .map(|session| history_to_responses_input(&session.history, &self.config.tool_registry))
            .transpose()
            .map_err(RootRequestBuildError::from)?
            .unwrap_or_default();
        let instructions = instructions_with_cwd(&self.config.instructions, &self.config.cwd);
        let body =
            ResponsesCreateRequest::new(self.config.model.clone(), self.config.model_capabilities)
                .with_instructions(instructions)
                .with_input(input)
                .with_tools(self.root_tool_registry().tools())
                .with_model_settings(&ModelSettings::new(
                    self.config.model.clone(),
                    self.config.reasoning_effort.clone(),
                    self.config.service_tier.clone(),
                ))
                .to_body()
                .map_err(RootRequestBuildError::from)?;
        Ok(ResponsesRequest {
            request_id: ResponsesRequestId::new(),
            headers: self.config.responses_headers.clone(),
            body,
        })
    }

    fn root_tool_registry(&self) -> NativeToolRegistry {
        self.config.tool_registry.clone()
    }

    async fn handle_mark_task_complete_call(&mut self) -> Option<NativeToolExecutionOutput> {
        if !self.root_persist_active {
            return Some(NativeToolExecutionOutput::same("persist not active"));
        }
        if self
            .root_persist_task
            .as_deref()
            .is_some_and(Self::persist_task_blocks_model_completion)
        {
            return Some(NativeToolExecutionOutput::same(
                "user requested persist not be ended",
            ));
        }

        self.root_persist_active = false;
        self.root_persist_task = None;
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(
                "persist: complete".to_string(),
            ))
            .await;
        self.complete_root_agentic_loop_if_idle().await;
        None
    }

    fn persist_task_blocks_model_completion(task: &str) -> bool {
        let task = task.to_ascii_lowercase();
        task.contains("do not mark")
            || task.contains("don't mark")
            || task.contains("dont mark")
            || task.contains("do not end")
            || task.contains("don't end")
            || task.contains("dont end")
            || task.contains("not be ended")
            || task.contains("keep working")
    }

    async fn emit_error(&mut self, message: String) {
        let _ = self
            .events
            .send(RuntimeEvent::TranscriptLine(format!("error: {message}")))
            .await;
    }

    async fn execute_root_freeform_tool_call(
        &mut self,
        handler: Option<NativeToolHandler>,
        call: FreeformToolCallRecord,
    ) -> NativeToolExecutionOutput {
        match handler {
            Some(NativeToolHandler::EditFile) => {
                execute_edit_file_tool(&self.config.cwd, &call.input)
            }
            Some(NativeToolHandler::Locate) => self.execute_locate_tool(call.input).await,
            Some(NativeToolHandler::Inspect) => {
                execute_inspect_tool(self.config.cwd.clone(), &call.input).await
            }
            Some(NativeToolHandler::StagedPatchApply) => {
                self.execute_staged_patch_apply_tool(&call.input)
            }
            Some(NativeToolHandler::StagedPatchDiscard) => {
                self.execute_staged_patch_discard_tool(&call.input)
            }
            _ => {
                execute_freeform_tool_call(
                    self.config.cwd.clone(),
                    handler,
                    self.terminal.clone(),
                    call,
                )
                .await
            }
        }
    }

    async fn execute_locate_tool(&mut self, input: String) -> NativeToolExecutionOutput {
        let query = match parse_locate_tool_input(&input) {
            Ok(query) => query,
            Err(message) => return NativeToolExecutionOutput::same(message),
        };
        let repository_map = match build_locator_repository_map(&self.config.cwd) {
            Ok(repository_map) => repository_map,
            Err(message) => return NativeToolExecutionOutput::same(message),
        };
        let agent_id = AgentId(NEXT_LOCATE_AGENT_ID.fetch_add(1, Ordering::Relaxed));
        let activity_id = format!("locate-{}", agent_id.0);
        self.emit_locate_activity(
            &activity_id,
            "running",
            Some(format!("locating: {}", locate_activity_query(&query))),
        )
        .await;
        let result = self
            .run_locate_worker(&query, &repository_map, agent_id)
            .await;
        let output = match result {
            Ok(output) if !output.trim().is_empty() => {
                self.emit_locate_activity(
                    &activity_id,
                    "completed",
                    Some("locate completed".to_string()),
                )
                .await;
                NativeToolExecutionOutput::same(output)
            }
            Ok(_) => {
                let message = "missing:\n- locator returned no output".to_string();
                self.emit_locate_activity(
                    &activity_id,
                    "completed",
                    Some("locate completed (no output)".to_string()),
                )
                .await;
                NativeToolExecutionOutput::same(message)
            }
            Err(message) => {
                self.emit_locate_activity(
                    &activity_id,
                    "failed",
                    Some(format!("locate error: {message}")),
                )
                .await;
                let _ = self
                    .events
                    .send(RuntimeEvent::TranscriptLine(format!(
                        "locate error: {message}"
                    )))
                    .await;
                NativeToolExecutionOutput::same(message)
            }
        };
        output
    }

    async fn run_locate_worker(
        &mut self,
        query: &str,
        repository_map: &str,
        agent_id: AgentId,
    ) -> Result<String, String> {
        let prompt_role = self.configured_input_role();
        let input = vec![
            message_input(prompt_role, LOCATE_WORKER_PROMPT)
                .map_err(|error| format!("failed to build locate worker instructions: {error}"))?,
            message_input(
                prompt_role,
                &format!("query:\n{query}\n\nrepository map:\n{repository_map}"),
            )
            .map_err(|error| format!("failed to build locate worker input: {error}"))?,
        ];
        self.collect_locate_worker_response(input, agent_id).await
    }

    async fn collect_locate_worker_response(
        &mut self,
        mut input: Vec<sonic_rs::Value>,
        agent_id: AgentId,
    ) -> Result<String, String> {
        // Resolve the transport and request settings to use for locate worker
        // requests. Locate always runs with pinned Ollama Cloud settings when
        // that transport is available so root-session model settings cannot
        // leak into the worker request.
        let (locate_transport, locate_settings, locate_capabilities, locate_kind) =
            if let Some(secondary) = self.secondary_responses().await {
                let runtime = self
                    .ollama_cloud_runtime
                    .as_ref()
                    .expect("secondary responses runtime is cached with the transport");
                (
                    secondary,
                    ModelSettings::new(
                        "glm-5.2".to_string(),
                        Some("high".to_string()),
                        runtime.selected_model.service_tier.clone(),
                    ),
                    // OllamaCloud models generally support tools without
                    // parallel calls; use a conservative capability set.
                    crate::responses::ResponsesModelCapabilities::new(false, false),
                    crate::providers::ProviderKind::OllamaCloud,
                )
            } else {
                (
                    self.responses.clone(),
                    ModelSettings::new(
                        "glm-5.2".to_string(),
                        Some("high".to_string()),
                        self.config.service_tier.clone(),
                    ),
                    self.config.model_capabilities,
                    self.config.provider_runtime.profile.kind,
                )
            };
        let tool_registry = locate_worker_tool_registry(locate_kind);
        let mut completed_tool_calls = 0usize;
        loop {
            let body =
                ResponsesCreateRequest::new(locate_settings.model.clone(), locate_capabilities)
                    .with_instructions(LOCATE_WORKER_REQUEST_INSTRUCTIONS)
                    .with_input(input.clone())
                    .with_tools(tool_registry.tools())
                    .with_model_settings(&locate_settings)
                    .to_body()
                    .map_err(|error| format!("failed to build locate worker request: {error}"))?;
            let request = ResponsesRequest {
                request_id: ResponsesRequestId::new(),
                headers: self.config.responses_headers.clone(),
                body,
            };
            let request_id = request.request_id;
            let (events_handle, events_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
            locate_transport
                .send(ProviderCommand::Submit {
                    request,
                    events: events_handle.sender(),
                })
                .await
                .map_err(|error| format!("failed to submit locate worker request: {error}"))?;

            let mut text = String::new();
            let mut pending_freeform_tool_inputs = HashMap::<String, String>::new();
            loop {
                let event = events_rx
                    .recv()
                    .await
                    .map_err(|error| format!("locate worker event channel closed: {error}"))?;
                match event {
                    ResponsesEvent::Frame {
                        request_id: event_request_id,
                        body,
                    } if event_request_id == request_id => {
                        if let Some(delta) = assistant_text_delta_from_frame(&body) {
                            text.push_str(delta);
                        }
                        if let Some(completed_text) = assistant_text_from_frame(&body) {
                            text = completed_text;
                        }
                    }
                    ResponsesEvent::FreeformToolInputDelta {
                        request_id: event_request_id,
                        delta,
                    } if event_request_id == request_id => {
                        let key = delta.call_id.unwrap_or_else(|| delta.item_id.clone());
                        pending_freeform_tool_inputs
                            .entry(key)
                            .or_default()
                            .push_str(&delta.delta);
                    }
                    ResponsesEvent::FreeformToolCall {
                        request_id: event_request_id,
                        call,
                    } if event_request_id == request_id => {
                        completed_tool_calls += 1;
                        if completed_tool_calls > LOCATE_WORKER_MAX_TOOL_CALLS {
                            return Err("locate worker exceeded tool call limit".to_string());
                        }
                        if !text.is_empty() {
                            input.push(message_input(MessageInputRole::Assistant, &text).map_err(
                                |error| {
                                    format!(
                                        "failed to build locate worker assistant message: {error}"
                                    )
                                },
                            )?);
                        }
                        let FreeformToolCall {
                            call_id,
                            name,
                            input: call_input,
                        } = call;
                        let call_input = if call_input.is_empty() {
                            pending_freeform_tool_inputs
                                .remove(&call_id)
                                .unwrap_or(call_input)
                        } else {
                            pending_freeform_tool_inputs.remove(&call_id);
                            call_input
                        };
                        let call_record = FreeformToolCallRecord {
                            call_id,
                            name,
                            input: call_input,
                        };
                        self.emit_locate_activity(
                            &format!("locate-{}", agent_id.0),
                            "running",
                            Some(locate_activity_tool_call(
                                &call_record.name,
                                &call_record.input,
                            )),
                        )
                        .await;

                        // After the force-response threshold, skip real tool
                        // execution and return a stop message as the tool
                        // output. This forces the model to write its answer
                        // on the next turn instead of issuing another call.
                        let output = if completed_tool_calls >= LOCATE_WORKER_FORCE_RESPONSE_AFTER {
                            NativeToolExecutionOutput::same(format!(
                                "[locate agent: you have made {} tool calls. \
                                 You MUST write your final answer now. \
                                 Do not call any more tools.]",
                                completed_tool_calls
                            ))
                        } else {
                            let handler = tool_registry.handler_for(&call_record.name);
                            let output = execute_freeform_tool_call(
                                self.config.cwd.clone(),
                                handler,
                                self.terminal.clone(),
                                call_record.clone(),
                            )
                            .await;
                            self.summarize_private_tool_output_if_needed(&call_record, output)
                                .await
                        };
                        let output_record =
                            freeform_tool_output_record(call_record.call_id.clone(), output);
                        let tool_registry = self.root_tool_registry();
                        let mut tool_items = history_to_responses_input(
                            &[
                                HistoryRecord::FreeformToolCall(call_record),
                                HistoryRecord::FreeformToolOutput(output_record),
                            ],
                            &tool_registry,
                        )
                        .map_err(|error| {
                            format!("failed to build locate worker tool output: {error}")
                        })?;
                        input.append(&mut tool_items);
                        break;
                    }
                    ResponsesEvent::RequestCompleted(event_request_id)
                        if event_request_id == request_id =>
                    {
                        return Ok(text);
                    }
                    ResponsesEvent::Error {
                        request_id: Some(event_request_id),
                        message,
                    } if event_request_id == request_id => {
                        return Err(format!("locate worker request failed: {message}"));
                    }
                    ResponsesEvent::Error {
                        request_id: None,
                        message,
                    } => return Err(format!("locate worker request failed: {message}")),
                    _ => {}
                }
            }
        }
    }

    async fn summarize_private_tool_output_if_needed(
        &mut self,
        call_record: &FreeformToolCallRecord,
        output: NativeToolExecutionOutput,
    ) -> NativeToolExecutionOutput {
        if !tool_output_needs_summary(call_record, &output) {
            return output;
        }

        match self
            .request_private_tool_output_summary(call_record, &output)
            .await
        {
            Ok(summary) => summarized_tool_output(output, &summary),
            Err(message) => tool_output_summary_failure_output(output, &message),
        }
    }

    async fn request_private_tool_output_summary(
        &mut self,
        call_record: &FreeformToolCallRecord,
        output: &NativeToolExecutionOutput,
    ) -> Result<String, String> {
        let context = self
            .active_session
            .as_ref()
            .map(|session| recent_tool_summary_context(&session.history))
            .unwrap_or_else(|| recent_tool_summary_context(&[]));
        let prompt = tool_output_summary_prompt(
            &context,
            &self.config.cwd,
            call_record,
            &output.model_output,
        );
        let input = vec![
            message_input(self.configured_input_role(), &prompt)
                .map_err(|error| format!("failed to build private tool summary prompt: {error}"))?,
        ];
        let summary_model = self
            .config
            .provider_runtime
            .profile
            .tool_output_summary_model
            .clone();
        let summary_reasoning_effort = self.config.reasoning_effort.clone();
        let body =
            ResponsesCreateRequest::new(summary_model.clone(), self.config.model_capabilities)
                .with_instructions(tool_output_summary_instructions())
                .with_input(input)
                .with_tools(Vec::new())
                .with_model_settings(&ModelSettings::new(
                    summary_model,
                    summary_reasoning_effort,
                    self.config.service_tier.clone(),
                ))
                .to_body()
                .map_err(|error| {
                    format!("failed to build private tool summary request: {error}")
                })?;
        let request = ResponsesRequest {
            request_id: ResponsesRequestId::new(),
            headers: self.config.responses_headers.clone(),
            body,
        };
        let request_id = request.request_id;
        let (events_handle, events_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
        self.responses
            .send(ProviderCommand::Submit {
                request,
                events: events_handle.sender(),
            })
            .await
            .map_err(|error| format!("failed to submit private tool summary request: {error}"))?;

        let mut summary = String::new();
        loop {
            let event = events_rx
                .recv()
                .await
                .map_err(|error| format!("private tool summary event channel closed: {error}"))?;
            match event {
                ResponsesEvent::Frame {
                    request_id: event_request_id,
                    body,
                } if event_request_id == request_id => {
                    if let Some(delta) = assistant_text_delta_from_frame(&body) {
                        summary.push_str(delta);
                    }
                }
                ResponsesEvent::RequestCompleted(event_request_id)
                    if event_request_id == request_id =>
                {
                    return Ok(normalized_tool_output_summary(&summary));
                }
                ResponsesEvent::Error {
                    request_id: Some(event_request_id),
                    message,
                } if event_request_id == request_id => {
                    return Err(format!("private tool summary request failed: {message}"));
                }
                ResponsesEvent::Error {
                    request_id: None,
                    message,
                } => return Err(format!("private tool summary request failed: {message}")),
                _ => {}
            }
        }
    }

    fn execute_staged_patch_apply_tool(&mut self, input: &str) -> NativeToolExecutionOutput {
        let patch_id = match parse_staged_patch_tool_input(input) {
            Ok(patch_id) => patch_id,
            Err(message) => return NativeToolExecutionOutput::same(message),
        };
        let Some(staged_patch) = self.staged_patches.get(&patch_id).cloned() else {
            return NativeToolExecutionOutput::same(format!(
                "staged patch `{patch_id}` does not exist"
            ));
        };
        match crate::apply_patch::apply_patch(&self.config.cwd, &staged_patch.patch) {
            Ok(output) => {
                self.staged_patches.remove(&patch_id);
                NativeToolExecutionOutput::same(format!("applied {patch_id}\n{output}"))
            }
            Err(error) => {
                NativeToolExecutionOutput::same(format!("failed to apply {patch_id}: {error}"))
            }
        }
    }

    fn execute_staged_patch_discard_tool(&mut self, input: &str) -> NativeToolExecutionOutput {
        let patch_id = match parse_staged_patch_tool_input(input) {
            Ok(patch_id) => patch_id,
            Err(message) => return NativeToolExecutionOutput::same(message),
        };
        if self.staged_patches.remove(&patch_id).is_some() {
            NativeToolExecutionOutput::same(format!("discarded {patch_id}"))
        } else {
            NativeToolExecutionOutput::same(format!("staged patch `{patch_id}` does not exist"))
        }
    }

    fn configured_input_role(&self) -> MessageInputRole {
        if self.config.developer_mode {
            MessageInputRole::Developer
        } else {
            MessageInputRole::User
        }
    }
}

const LOCATE_WORKER_PROMPT: &str = r#"You are a locate agent. Find specific context in the codebase and return it.

How to work:
- Use inspect `search ...` for symbols, identifiers, config keys, and text patterns.
- Use inspect `read <path>` with specific line ranges to confirm context; do not read whole files.
- Use inspect `pwd` and `ps [name]` when process or cwd context is required.
- If the evidence is insufficient, write what is missing.

Write your answer when done. Include file paths, line numbers, and the relevant code or context. Keep it brief and factual. Do not suggest implementations or plans."#;

const LOCATE_WORKER_REQUEST_INSTRUCTIONS: &str = "Find context in the codebase. Write your answer when you have enough. Keep it factual and brief.";

/// After this many tool calls the locate worker injects a stop message as the
/// tool result so the model is forced to write its final answer instead of
/// making another tool call.
const LOCATE_WORKER_FORCE_RESPONSE_AFTER: usize = 20;

/// Hard abort: if the model still hasn't produced a final text response after
/// this many tool calls, error out.
const LOCATE_WORKER_MAX_TOOL_CALLS: usize = 30;
const LOCATOR_MAX_FILES: usize = 400;
const LOCATOR_MAX_BYTES: usize = 80_000;
const LOCATOR_MAX_SYMBOLS_PER_FILE: usize = 24;

/// Maximum number of characters retained from the locate query when rendering
/// the locate subagent activity line.
const LOCATE_ACTIVITY_QUERY_MAX_CHARS: usize = 80;
/// Maximum number of characters retained from a private tool call input when
/// rendering the locate subagent activity line.
const LOCATE_ACTIVITY_TOOL_INPUT_MAX_CHARS: usize = 80;

/// Build a compact one-line summary of the locate query for the activity panel.
fn locate_activity_query(query: &str) -> String {
    let normalized = query.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&normalized, LOCATE_ACTIVITY_QUERY_MAX_CHARS)
}

/// Build a compact one-line summary of a locate worker private tool call for
/// the activity panel.
fn locate_activity_tool_call(name: &str, input: &str) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = truncate_chars(&normalized, LOCATE_ACTIVITY_TOOL_INPUT_MAX_CHARS);
    format!("{name}: {truncated}")
}

/// Truncate `text` to at most `max` characters on a character boundary, appending
/// an ellipsis when truncation occurs.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut output = text.chars().take(max.saturating_sub(1)).collect::<String>();
    output.push('…');
    output
}
fn locate_worker_tool_registry(kind: crate::providers::ProviderKind) -> NativeToolRegistry {
    let registry = NativeToolRegistry::new(vec![crate::tools::NativeToolEntry::new(
        NativeTool::inspect(),
        NativeToolHandler::Inspect,
    )]);

    match kind {
        crate::providers::ProviderKind::OllamaCloud => {
            registry.with_function_wrapped_freeform_tools()
        }
        crate::providers::ProviderKind::Codex | crate::providers::ProviderKind::HttpsApi => {
            registry
        }
    }
}

fn format_provider_profile_list(
    config: &crate::providers::ProviderConfig,
    active_profile_id: &ProviderProfileId,
) -> String {
    let mut output = String::from("providers:");
    for profile in &config.profiles {
        let active_marker = if &profile.id == active_profile_id {
            " active"
        } else {
            ""
        };
        let default_marker = if profile.id == config.default_profile_id {
            " default"
        } else {
            ""
        };
        let _ = write!(
            output,
            "\n- {}{}{}: {} kind={} transport={} base_url={} default_model={} summary_model={}",
            profile.id.as_str(),
            active_marker,
            default_marker,
            profile.display_name,
            profile.kind.label(),
            profile.driver.transport().label(),
            profile.driver.base_url(),
            profile.default_model,
            profile.tool_output_summary_model
        );
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditFileToolRequest {
    operations: Vec<EditFileOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditFileOperation {
    Add {
        path: String,
        body: String,
    },
    Remove {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
    Edit {
        path: String,
        segments: Vec<EditSegment>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditSegment {
    Replace {
        start: LineAnchor,
        end: LineAnchor,
        body: String,
    },
    Delete {
        start: LineAnchor,
        end: LineAnchor,
    },
    Insert {
        position: InsertPosition,
        anchor: LineAnchor,
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectReadOutputRequest {
    path: String,
    start_line: usize,
    line_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectRequest {
    jobs: Vec<InspectJob>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InspectJob {
    Read(InspectReadJob),
    Command(InspectCommandJob),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectReadJob {
    path: String,
    ranges: Vec<InspectReadRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InspectReadRange {
    start_line: usize,
    line_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectCommandJob {
    command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathCommandMatch {
    name: String,
    path: PathBuf,
    score: (u8, usize, usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectJobResult {
    job: InspectJob,
    output: NativeToolExecutionOutput,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct FffSearchRequest {
    pattern: String,
    query_constraints: Vec<String>,
    exact_path_filters: Vec<String>,
    excludes: Vec<String>,
    search_roots: Vec<FffSearchRoot>,
    mode: GrepMode,
    smart_case: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FffFilesRequest {
    query_constraints: Vec<String>,
    exact_path_filters: Vec<String>,
    excludes: Vec<String>,
    search_roots: Vec<FffSearchRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FffCommand {
    Search(FffSearchRequest),
    Files(FffFilesRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FffSearchRoot {
    path: PathBuf,
    label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FffPipelineStage {
    Fff(FffCommand),
    Head(usize),
    Tail(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineAnchor {
    line_number: usize,
    hash: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertPosition {
    Before,
    After,
    Append,
}

fn parse_locate_tool_input(input: &str) -> Result<String, String> {
    let query = input.trim();
    if query.is_empty() {
        return Err("failed to parse `locate` input: query must not be empty".to_string());
    }
    Ok(query.to_string())
}

fn format_inspect_read_output(request: &InspectReadOutputRequest, text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    if request.start_line > total_lines {
        return format!("no lines; file has {total_lines} lines\n");
    }

    let start_index = request.start_line - 1;
    let end_index = total_lines.min(start_index + request.line_count);
    let first_line = request.start_line;
    let mut output = String::new();
    for (index, line) in lines[start_index..end_index].iter().enumerate() {
        let line_number = first_line + index;
        let anchor = format_line_anchor(line_number, edit_line_hash(line));
        let _ = writeln!(output, "{anchor}{line}");
    }
    if end_index < total_lines {
        let _ = writeln!(output, "next: {}+{}", end_index + 1, request.line_count);
    }
    output
}

fn format_inspect_read_display(
    request: &InspectReadOutputRequest,
    text: &str,
) -> InspectReadDisplayRecord {
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    if request.start_line > total_lines {
        return InspectReadDisplayRecord {
            path: request.path.clone(),
            start_line: request.start_line,
            lines: Vec::new(),
            next: None,
        };
    }

    let start_index = request.start_line - 1;
    let end_index = total_lines.min(start_index + request.line_count);
    let next = (end_index < total_lines).then_some(InspectReadNextRecord {
        start_line: end_index + 1,
        line_count: request.line_count,
    });
    InspectReadDisplayRecord {
        path: request.path.clone(),
        start_line: request.start_line,
        lines: lines[start_index..end_index]
            .iter()
            .map(|line| (*line).to_string())
            .collect(),
        next,
    }
}

fn execute_pwd_tool(cwd: &str, input: &str) -> NativeToolExecutionOutput {
    if input.trim() != "pwd" || input.lines().count() > 1 {
        return NativeToolExecutionOutput::same(
            "failed to parse `inspect` pwd input: expected exactly `pwd`".to_string(),
        );
    }
    NativeToolExecutionOutput::same(format!("{cwd}\n"))
}

fn format_rounded_file_size(size: u64) -> String {
    const UNITS: [(&str, u64); 5] = [
        ("bytes", 1),
        ("KB", 1_000),
        ("MB", 1_000_000),
        ("GB", 1_000_000_000),
        ("TB", 1_000_000_000_000),
    ];

    let mut unit_index = UNITS
        .iter()
        .rposition(|(_, factor)| size >= *factor)
        .unwrap_or(0);
    let mut rounded = if unit_index == 0 {
        size
    } else {
        let factor = UNITS[unit_index].1;
        size.saturating_add(factor / 2) / factor
    };
    if rounded == 1_000 && unit_index + 1 < UNITS.len() {
        unit_index += 1;
        rounded = 1;
    }
    format!("{rounded} {}", UNITS[unit_index].0)
}

fn format_file_size(size: u64, exact: bool) -> String {
    if exact {
        format!("{size} bytes")
    } else {
        format_rounded_file_size(size)
    }
}

fn text_file_line_count(path: &Path) -> Result<Option<u64>, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut chunk = [0_u8; 8_192];
    let mut utf8_pending = Vec::with_capacity(chunk.len() + 3);
    let mut line_feeds = 0_u64;
    let mut total_bytes = 0_u64;
    let mut last_byte = None;

    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        let bytes = &chunk[..read];
        total_bytes += read as u64;
        line_feeds += bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
        last_byte = bytes.last().copied();
        if bytes.contains(&0) {
            return Ok(None);
        }

        utf8_pending.extend_from_slice(bytes);
        match std::str::from_utf8(&utf8_pending) {
            Ok(_) => utf8_pending.clear(),
            Err(error) if error.error_len().is_some() => return Ok(None),
            Err(error) => {
                let incomplete_start = error.valid_up_to();
                utf8_pending.copy_within(incomplete_start.., 0);
                utf8_pending.truncate(utf8_pending.len() - incomplete_start);
            }
        }
    }

    if !utf8_pending.is_empty() {
        return Ok(None);
    }
    let lines = line_feeds + u64::from(total_bytes > 0 && last_byte != Some(b'\n'));
    Ok(Some(lines))
}

fn format_line_count(lines: u64) -> String {
    if lines == 1 {
        "1 line".to_string()
    } else {
        format!("{lines} lines")
    }
}

fn append_directory_suffix(path: &mut String) {
    if !path.ends_with('/') {
        path.push('/');
    }
}

fn format_special_file_kind(file_type: fs::FileType) -> &'static str {
    if file_type.is_socket() {
        "socket"
    } else if file_type.is_fifo() {
        "pipe"
    } else if file_type.is_block_device() {
        "block device"
    } else if file_type.is_char_device() {
        "character device"
    } else {
        "special"
    }
}

fn format_inspect_path(
    display_path: &Path,
    resolved_path: &Path,
    metadata: &fs::Metadata,
) -> Result<String, String> {
    let file_type = metadata.file_type();
    let mut source = display_path.to_string_lossy().to_string();
    if file_type.is_dir() {
        append_directory_suffix(&mut source);
        return Ok(source);
    }
    if !file_type.is_symlink() {
        if file_type.is_file() {
            return Ok(source);
        }
        return Ok(format!(
            "{source} [{}]",
            format_special_file_kind(file_type)
        ));
    }

    let target = fs::read_link(resolved_path).map_err(|error| {
        format!(
            "failed to read symlink {}: {error}",
            resolved_path.display()
        )
    })?;
    let (target_is_directory, broken) = match fs::metadata(resolved_path) {
        Ok(target_metadata) => (target_metadata.is_dir(), false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (false, true),
        Err(error) => {
            return Err(format!(
                "failed to inspect symlink target {}: {error}",
                resolved_path.display()
            ));
        }
    };
    if target_is_directory {
        append_directory_suffix(&mut source);
    }
    let mut target_display = target.to_string_lossy().to_string();
    if target_is_directory {
        append_directory_suffix(&mut target_display);
    }
    let broken = if broken { " [broken]" } else { "" };
    Ok(format!("{source} -> {target_display}{broken}"))
}

fn execute_list_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let words = match parse_shell_words(command) {
        Ok(words) => words,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` list input: {message}"
            )));
        }
    };
    let mut path = None;
    let mut depth = 1_usize;
    let mut exact = false;
    let mut index = 1;
    while index < words.len() {
        match words[index].value.as_str() {
            "--depth" if index + 1 < words.len() => {
                depth = match parse_positive_usize("list --depth", &words[index + 1].value) {
                    Ok(depth) => depth,
                    Err(message) => {
                        return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                            "failed to parse `inspect` list input: {message}"
                        )));
                    }
                };
                index += 2;
            }
            "--exact" if !exact => {
                exact = true;
                index += 1;
            }
            option if option.starts_with('-') => {
                return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                    "failed to parse `inspect` list input: unsupported option `{option}`"
                )));
            }
            value if path.is_none() => {
                path = Some(value.to_string());
                index += 1;
            }
            value => {
                return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                    "failed to parse `inspect` list input: unexpected argument `{value}`"
                )));
            }
        }
    }
    let path = path.unwrap_or_else(|| ".".to_string());
    let resolved = resolve_root_relative_path(cwd, &path);
    if !resolved.is_dir() {
        return inspect_command_failed(NativeToolExecutionOutput::same(format!(
            "failed to list {}: not a directory",
            resolved.display()
        )));
    }

    let mut output = String::new();
    let mut remaining_entries = INSPECT_LIST_ENTRY_LIMIT;
    let mut truncated = false;
    match append_inspect_directory_listing(
        &resolved,
        Path::new(""),
        1,
        depth,
        exact,
        &mut remaining_entries,
        &mut truncated,
        &mut output,
    ) {
        Ok(()) => {
            if truncated {
                let _ = writeln!(
                    output,
                    "[list output truncated: showing first {INSPECT_LIST_ENTRY_LIMIT} entries; use a narrower path or depth]"
                );
            }
            inspect_command_succeeded(NativeToolExecutionOutput::same(output))
        }
        Err(message) => inspect_command_failed(NativeToolExecutionOutput::same(message)),
    }
}

fn append_inspect_directory_listing(
    directory: &Path,
    relative_directory: &Path,
    current_depth: usize,
    max_depth: usize,
    exact: bool,
    remaining_entries: &mut usize,
    truncated: &mut bool,
    output: &mut String,
) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to list {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to list {}: {error}", directory.display()))?;
    let mut entries = entries;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        if *remaining_entries == 0 {
            *truncated = true;
            return Ok(());
        }
        *remaining_entries -= 1;
        let resolved_path = entry.path();
        let relative_path = relative_directory.join(entry.file_name());
        let metadata = fs::symlink_metadata(&resolved_path)
            .map_err(|error| format!("failed to inspect {}: {error}", resolved_path.display()))?;
        let path_display = format_inspect_path(&relative_path, &resolved_path, &metadata)?;
        output.push_str(&path_display);
        if metadata.is_file() {
            let size = metadata.len();
            match text_file_line_count(&resolved_path)? {
                Some(lines) => {
                    let _ = write!(output, " {}", format_line_count(lines));
                    if exact {
                        let _ = write!(output, " {size} bytes");
                    }
                }
                None => {
                    let _ = write!(output, " {}", format_file_size(size, exact));
                }
            }
        }
        output.push('\n');

        if metadata.is_dir() && current_depth < max_depth {
            append_inspect_directory_listing(
                &resolved_path,
                &relative_path,
                current_depth + 1,
                max_depth,
                exact,
                remaining_entries,
                truncated,
                output,
            )?;
        }
    }
    Ok(())
}

fn format_permissions(mode: u32) -> String {
    format!("{:04o}", mode & 0o7777)
}

fn format_unix_timestamp(seconds: i64, nanoseconds: i64, exact: bool) -> String {
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let shifted_days = days + 719_468;
    let era = if shifted_days >= 0 {
        shifted_days
    } else {
        shifted_days - 146_096
    } / 146_097;
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_piece = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_piece + 2) / 5 + 1;
    let month = month_piece + if month_piece < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    if exact {
        format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{nanoseconds:09} UTC"
        )
    } else {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
    }
}

fn execute_stat_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let words = match parse_shell_words(command) {
        Ok(words) => words,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` stat input: {message}"
            )));
        }
    };
    let mut paths = Vec::new();
    let mut exact = false;
    let mut extended_metadata = false;
    for word in &words[1..] {
        match word.value.as_str() {
            "--exact" if !exact => exact = true,
            "--metadata" if !extended_metadata => extended_metadata = true,
            option if option.starts_with('-') => {
                return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                    "failed to parse `inspect` stat input: unsupported option `{option}`"
                )));
            }
            path => paths.push(path.to_string()),
        }
    }
    if paths.is_empty() {
        return inspect_command_failed(NativeToolExecutionOutput::same(
            "failed to parse `inspect` input: usage: `stat <path> [path ...] [--exact] [--metadata]`"
                .to_string(),
        ));
    }

    let mut output = String::new();
    for (index, path) in paths.iter().enumerate() {
        let resolved = resolve_root_relative_path(cwd, path);
        let metadata = match fs::symlink_metadata(&resolved) {
            Ok(metadata) => metadata,
            Err(error) => {
                return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                    "failed to stat {}: {error}",
                    resolved.display()
                )));
            }
        };
        if index > 0 {
            output.push('\n');
        }
        let identity = match format_inspect_path(Path::new(path), &resolved, &metadata) {
            Ok(identity) => identity,
            Err(message) => {
                return inspect_command_failed(NativeToolExecutionOutput::same(message));
            }
        };
        let _ = writeln!(output, "{identity}");
        let _ = writeln!(output, "size: {}", format_file_size(metadata.len(), exact));
        let _ = writeln!(
            output,
            "modified: {}",
            format_unix_timestamp(metadata.mtime(), metadata.mtime_nsec(), exact)
        );
        let _ = writeln!(
            output,
            "permissions: {}",
            format_permissions(metadata.mode())
        );
        if extended_metadata {
            let _ = writeln!(output, "uid: {}", metadata.uid());
            let _ = writeln!(output, "gid: {}", metadata.gid());
            let _ = writeln!(output, "inode: {}", metadata.ino());
            let _ = writeln!(output, "device: {}", metadata.dev());
            let _ = writeln!(output, "links: {}", metadata.nlink());
            let _ = writeln!(
                output,
                "accessed: {}",
                format_unix_timestamp(metadata.atime(), metadata.atime_nsec(), exact)
            );
            let _ = writeln!(
                output,
                "changed: {}",
                format_unix_timestamp(metadata.ctime(), metadata.ctime_nsec(), exact)
            );
            let _ = writeln!(output, "blocks: {}", metadata.blocks());
            let _ = writeln!(output, "block size: {}", metadata.blksize());
        }
    }
    inspect_command_succeeded(NativeToolExecutionOutput::same(output))
}

fn parse_byte_range(value: &str) -> Result<(u64, usize), String> {
    let Some((offset, length)) = value.split_once('+') else {
        return Err("range must be `offset+length`".to_string());
    };
    let offset = offset
        .parse::<u64>()
        .map_err(|_| "offset must be a non-negative integer".to_string())?;
    let length = length
        .parse::<usize>()
        .map_err(|_| "length must be a positive integer".to_string())?;
    if length == 0 {
        return Err("length must be a positive integer".to_string());
    }
    if length > INSPECT_BYTES_MAX_LENGTH {
        return Err(format!(
            "length must not exceed {INSPECT_BYTES_MAX_LENGTH} bytes"
        ));
    }
    Ok((offset, length))
}

fn encode_contiguous_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn execute_bytes_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let words = match parse_shell_words(command) {
        Ok(words) => words,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` bytes input: {message}"
            )));
        }
    };
    let mut positional = Vec::new();
    for word in &words[1..] {
        match word.value.as_str() {
            option if option.starts_with('-') => {
                return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                    "failed to parse `inspect` bytes input: unsupported option `{option}`"
                )));
            }
            value => positional.push(value.to_string()),
        }
    }
    let [path, range] = positional.as_slice() else {
        return inspect_command_failed(NativeToolExecutionOutput::same(
            "failed to parse `inspect` input: usage: `bytes <path> <offset>+<length>`".to_string(),
        ));
    };
    let (offset, requested_length) = match parse_byte_range(range) {
        Ok(range) => range,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` bytes input: {message}"
            )));
        }
    };
    let resolved = resolve_root_relative_path(cwd, path);
    let mut file = match fs::File::open(&resolved) {
        Ok(file) => file,
        Err(error) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to read {}: {error}",
                resolved.display()
            )));
        }
    };
    let size = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to read {}: not a regular file",
                resolved.display()
            )));
        }
        Err(error) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to inspect {}: {error}",
                resolved.display()
            )));
        }
    };
    if offset > size {
        return inspect_command_failed(NativeToolExecutionOutput::same(format!(
            "failed to read {}: offset {offset} is beyond file size {size}",
            resolved.display()
        )));
    }
    if let Err(error) = file.seek(SeekFrom::Start(offset)) {
        return inspect_command_failed(NativeToolExecutionOutput::same(format!(
            "failed to seek {}: {error}",
            resolved.display()
        )));
    }
    let actual_length = requested_length.min((size - offset) as usize);
    let mut bytes = vec![0_u8; actual_length];
    if let Err(error) = file.read_exact(&mut bytes) {
        return inspect_command_failed(NativeToolExecutionOutput::same(format!(
            "failed to read {}: {error}",
            resolved.display()
        )));
    }

    let mut output = format!("{path} {}\n", format_file_size(size, false));
    let _ = writeln!(output, "range: {offset}+{actual_length}");
    let _ = writeln!(output, "{}", encode_contiguous_hex(&bytes));
    let next_offset = offset + actual_length as u64;
    if next_offset < size {
        let _ = writeln!(output, "next: {next_offset}+{requested_length}");
    }
    inspect_command_succeeded(NativeToolExecutionOutput::same(output))
}
fn decode_contiguous_hex(input: &str) -> Result<Vec<u8>, String> {
    if input.is_empty() {
        return Err("hex sequence must not be empty".to_string());
    }
    if !input.len().is_multiple_of(2) {
        return Err("hex sequence must contain an even number of digits".to_string());
    }
    let mut bytes = Vec::with_capacity(input.len() / 2);
    for pair in input.as_bytes().chunks_exact(2) {
        let high = hex_digit_value(pair[0])
            .ok_or_else(|| "hex sequence contains a non-hexadecimal digit".to_string())?;
        let low = hex_digit_value(pair[1])
            .ok_or_else(|| "hex sequence contains a non-hexadecimal digit".to_string())?;
        bytes.push(high << 4 | low);
    }
    Ok(bytes)
}

fn hex_digit_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn execute_byte_search_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let words = match parse_shell_words(command) {
        Ok(words) => words,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` byte-search input: {message}"
            )));
        }
    };
    let [_, path, hex] = words.as_slice() else {
        return inspect_command_failed(NativeToolExecutionOutput::same(
            "failed to parse `inspect` input: usage: `byte-search <path> <hex>`".to_string(),
        ));
    };
    let pattern = match decode_contiguous_hex(&hex.value) {
        Ok(pattern) => pattern,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` byte-search input: {message}"
            )));
        }
    };
    let resolved = resolve_root_relative_path(cwd, &path.value);
    let file = match fs::File::open(&resolved) {
        Ok(file) => file,
        Err(error) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to read {}: {error}",
                resolved.display()
            )));
        }
    };

    let prefix = byte_pattern_prefix(&pattern);

    let mut reader = BufReader::new(file);
    let mut chunk = [0_u8; 16_384];
    let mut position = 0_u64;
    let mut matched = 0_usize;
    let mut match_count = 0_usize;
    let mut output = String::new();
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(error) => {
                return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                    "failed to read {}: {error}",
                    resolved.display()
                )));
            }
        };
        if read == 0 {
            break;
        }
        for byte in &chunk[..read] {
            while matched > 0 && *byte != pattern[matched] {
                matched = prefix[matched - 1];
            }
            if *byte == pattern[matched] {
                matched += 1;
            }
            if matched == pattern.len() {
                let offset = position + 1 - pattern.len() as u64;
                if match_count < INSPECT_INDEX_RESULT_LIMIT {
                    let _ = writeln!(output, "{offset}");
                }
                match_count += 1;
                matched = prefix[matched - 1];
            }
            position += 1;
        }
    }

    if match_count == 0 {
        output.push_str("no results\n");
    } else if match_count > INSPECT_INDEX_RESULT_LIMIT {
        let _ = writeln!(
            output,
            "[byte-search output truncated: showing first {INSPECT_INDEX_RESULT_LIMIT} of {match_count} offsets]"
        );
    }
    inspect_command_succeeded(NativeToolExecutionOutput::same(output))
}

fn utf8_sequence_length(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

struct IndexedStringRun {
    offset: Option<u64>,
    character_count: usize,
    preview: String,
    filter_matched: usize,
    filter_found: bool,
}

impl IndexedStringRun {
    fn new(literal: Option<&str>) -> Self {
        Self {
            offset: None,
            character_count: 0,
            preview: String::new(),
            filter_matched: 0,
            filter_found: literal.is_none_or(str::is_empty),
        }
    }

    fn push(
        &mut self,
        offset: u64,
        character: char,
        literal: Option<&str>,
        filter_prefix: &[usize],
    ) {
        self.offset.get_or_insert(offset);
        self.character_count += 1;
        if self.character_count <= INSPECT_STRING_PREVIEW_CHARS {
            self.preview.push(character);
        }
        let Some(pattern) = literal
            .filter(|literal| !literal.is_empty())
            .map(str::as_bytes)
        else {
            return;
        };
        if self.filter_found {
            return;
        }
        let mut encoded = [0_u8; 4];
        for byte in character.encode_utf8(&mut encoded).bytes() {
            while self.filter_matched > 0 && byte != pattern[self.filter_matched] {
                self.filter_matched = filter_prefix[self.filter_matched - 1];
            }
            if byte == pattern[self.filter_matched] {
                self.filter_matched += 1;
            }
            if self.filter_matched == pattern.len() {
                self.filter_found = true;
                self.filter_matched = 0;
                break;
            }
        }
    }

    fn flush(&mut self, output: &mut String, match_count: &mut usize, literal: Option<&str>) {
        if self.character_count >= 4 && self.filter_found {
            if *match_count < INSPECT_INDEX_RESULT_LIMIT {
                let _ = writeln!(
                    output,
                    "{} {}{}",
                    self.offset.unwrap(),
                    self.preview,
                    if self.character_count > INSPECT_STRING_PREVIEW_CHARS {
                        "…"
                    } else {
                        ""
                    }
                );
            }
            *match_count += 1;
        }
        *self = Self::new(literal);
    }
}

fn byte_pattern_prefix(pattern: &[u8]) -> Vec<usize> {
    let mut prefix = vec![0_usize; pattern.len()];
    for index in 1..pattern.len() {
        let mut matched = prefix[index - 1];
        while matched > 0 && pattern[index] != pattern[matched] {
            matched = prefix[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }
    prefix
}

fn execute_strings_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let words = match parse_shell_words(command) {
        Ok(words) => words,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` strings input: {message}"
            )));
        }
    };
    let (path, literal) = match words.as_slice() {
        [_, path] => (&path.value, None),
        [_, path, literal] => (&path.value, Some(literal.value.as_str())),
        _ => {
            return inspect_command_failed(NativeToolExecutionOutput::same(
                "failed to parse `inspect` input: usage: `strings <path> [literal]`".to_string(),
            ));
        }
    };
    let resolved = resolve_root_relative_path(cwd, path);
    let file = match fs::File::open(&resolved) {
        Ok(file) => file,
        Err(error) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to read {}: {error}",
                resolved.display()
            )));
        }
    };
    let filter_prefix =
        literal.map_or_else(Vec::new, |literal| byte_pattern_prefix(literal.as_bytes()));

    let mut reader = BufReader::new(file);
    let mut chunk = [0_u8; 16_384];
    let mut candidate = Vec::with_capacity(4);
    let mut candidate_length = 0_usize;
    let mut run = IndexedStringRun::new(literal);
    let mut position = 0_u64;
    let mut match_count = 0_usize;
    let mut output = String::new();

    loop {
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(error) => {
                return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                    "failed to read {}: {error}",
                    resolved.display()
                )));
            }
        };
        if read == 0 {
            break;
        }
        for byte in &chunk[..read] {
            let mut current = Some(*byte);
            while let Some(byte) = current.take() {
                if candidate.is_empty() {
                    if (b' '..=b'~').contains(&byte) {
                        run.push(position, char::from(byte), literal, &filter_prefix);
                    } else if byte.is_ascii() {
                        run.flush(&mut output, &mut match_count, literal);
                    } else if let Some(length) = utf8_sequence_length(byte) {
                        candidate.push(byte);
                        candidate_length = length;
                    } else {
                        run.flush(&mut output, &mut match_count, literal);
                    }
                } else if byte & 0xc0 == 0x80 {
                    candidate.push(byte);
                    if candidate.len() == candidate_length {
                        let character = std::str::from_utf8(&candidate)
                            .ok()
                            .and_then(|text| text.chars().next());
                        if let Some(character) =
                            character.filter(|character| !character.is_control())
                        {
                            let character_offset = position + 1 - candidate.len() as u64;
                            run.push(character_offset, character, literal, &filter_prefix);
                        } else {
                            run.flush(&mut output, &mut match_count, literal);
                        }
                        candidate.clear();
                    }
                } else {
                    run.flush(&mut output, &mut match_count, literal);
                    candidate.clear();
                    current = Some(byte);
                }
            }
            position += 1;
        }
    }
    run.flush(&mut output, &mut match_count, literal);

    if match_count == 0 {
        output.push_str("no results\n");
    } else if match_count > INSPECT_INDEX_RESULT_LIMIT {
        let _ = writeln!(
            output,
            "[strings output truncated: showing first {INSPECT_INDEX_RESULT_LIMIT} of {match_count} strings; pass a literal filter]"
        );
    }
    inspect_command_succeeded(NativeToolExecutionOutput::same(output))
}

fn parse_non_negative_u64(value: &str, label: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{label} must be a non-negative integer"))
}

fn elf_section_permissions(flags: object::SectionFlags) -> String {
    let object::SectionFlags::Elf { sh_flags } = flags else {
        return String::new();
    };
    let mut permissions = Vec::new();
    if sh_flags & u64::from(object::elf::SHF_ALLOC) != 0 {
        permissions.push("allocated");
    }
    if sh_flags & u64::from(object::elf::SHF_WRITE) != 0 {
        permissions.push("writable");
    } else if sh_flags & u64::from(object::elf::SHF_ALLOC) != 0 {
        permissions.push("read-only");
    }
    if sh_flags & u64::from(object::elf::SHF_EXECINSTR) != 0 {
        permissions.push("executable");
    }
    permissions.join(" ")
}

fn elf_segment_permissions(flags: object::SegmentFlags) -> String {
    let object::SegmentFlags::Elf { p_flags } = flags else {
        return String::new();
    };
    let mut permissions = String::new();
    if p_flags & object::elf::PF_R != 0 {
        permissions.push('r');
    }
    if p_flags & object::elf::PF_W != 0 {
        permissions.push('w');
    }
    if p_flags & object::elf::PF_X != 0 {
        permissions.push('x');
    }
    permissions
}

fn elf_section_is_allocated(flags: object::SectionFlags) -> bool {
    matches!(
        flags,
        object::SectionFlags::Elf { sh_flags }
            if sh_flags & u64::from(object::elf::SHF_ALLOC) != 0
    )
}

fn elf_virtual_to_file<'data, O: object::Object<'data>>(
    object: &O,
    address: u64,
) -> Option<(u64, String)> {
    for section in object.sections() {
        if !elf_section_is_allocated(section.flags()) {
            continue;
        }
        let Some((file_offset, file_size)) = section.file_range() else {
            continue;
        };
        let Some(delta) = address.checked_sub(section.address()) else {
            continue;
        };
        if delta < file_size {
            return Some((
                file_offset + delta,
                section.name().unwrap_or("<invalid-name>").to_string(),
            ));
        }
    }
    for (index, segment) in object.segments().enumerate() {
        let (file_offset, file_size) = segment.file_range();
        let Some(delta) = address.checked_sub(segment.address()) else {
            continue;
        };
        if delta < file_size {
            return Some((file_offset + delta, format!("segment {index}")));
        }
    }
    None
}

fn elf_file_to_virtual<'data, O: object::Object<'data>>(
    object: &O,
    offset: u64,
) -> Option<(u64, String)> {
    for section in object.sections() {
        if !elf_section_is_allocated(section.flags()) {
            continue;
        }
        let Some((file_offset, file_size)) = section.file_range() else {
            continue;
        };
        let Some(delta) = offset.checked_sub(file_offset) else {
            continue;
        };
        if delta < file_size {
            return Some((
                section.address() + delta,
                section.name().unwrap_or("<invalid-name>").to_string(),
            ));
        }
    }
    for (index, segment) in object.segments().enumerate() {
        let (file_offset, file_size) = segment.file_range();
        let Some(delta) = offset.checked_sub(file_offset) else {
            continue;
        };
        if delta < file_size {
            return Some((segment.address() + delta, format!("segment {index}")));
        }
    }
    None
}

fn write_elf_sections<'data, O: object::Object<'data>>(object: &O, output: &mut String) {
    for (index, section) in object.sections().enumerate() {
        if index == INSPECT_INDEX_RESULT_LIMIT {
            let _ = writeln!(
                output,
                "[elf sections truncated: showing first {INSPECT_INDEX_RESULT_LIMIT} sections]"
            );
            return;
        }
        let name = section.name().unwrap_or("<invalid-name>");
        let _ = write!(output, "{name}");
        if let Some((offset, size)) = section.file_range() {
            let _ = write!(output, " file {offset}+{size}");
        } else {
            let _ = write!(output, " no-file-data");
        }
        let _ = write!(output, " virtual {}+{}", section.address(), section.size());
        let permissions = elf_section_permissions(section.flags());
        if !permissions.is_empty() {
            let _ = write!(output, " {permissions}");
        }
        output.push('\n');
    }
}

fn write_elf_segments<'data, O: object::Object<'data>>(object: &O, output: &mut String) {
    for (index, segment) in object.segments().enumerate() {
        if index == INSPECT_INDEX_RESULT_LIMIT {
            let _ = writeln!(
                output,
                "[elf segments truncated: showing first {INSPECT_INDEX_RESULT_LIMIT} segments]"
            );
            return;
        }
        let (offset, file_size) = segment.file_range();
        let permissions = elf_segment_permissions(segment.flags());
        let _ = writeln!(
            output,
            "segment {index} file {offset}+{file_size} virtual {}+{} {permissions}",
            segment.address(),
            segment.size()
        );
    }
}

fn elf_symbol_file_mapping<'data, O: object::Object<'data>>(
    object: &O,
    symbol: &O::Symbol<'_>,
) -> Option<(u64, String)> {
    if let Some(section_index) = symbol.section_index()
        && let Ok(section) = object.section_by_index(section_index)
        && let Some((file_offset, file_size)) = section.file_range()
    {
        let delta = symbol.address().checked_sub(section.address())?;
        if delta < file_size {
            return Some((
                file_offset + delta,
                section.name().unwrap_or("<invalid-name>").to_string(),
            ));
        }
    }
    elf_virtual_to_file(object, symbol.address())
}

fn write_elf_symbols<'data, O: object::Object<'data>>(
    object: &O,
    literal: Option<&str>,
    output: &mut String,
) {
    let mut seen = HashSet::new();
    let mut match_count = 0_usize;
    for symbol in object.symbols().chain(object.dynamic_symbols()) {
        let Ok(name) = symbol.name() else {
            continue;
        };
        if name.is_empty() || literal.is_some_and(|literal| !name.contains(literal)) {
            continue;
        }
        if !seen.insert((
            name.to_string(),
            symbol.address(),
            symbol.size(),
            symbol.is_undefined(),
        )) {
            continue;
        }
        if match_count < INSPECT_INDEX_RESULT_LIMIT {
            if symbol.is_undefined() {
                let _ = writeln!(output, "{name} undefined {:?}", symbol.kind());
            } else {
                let _ = write!(
                    output,
                    "{name} virtual {}+{} {:?}",
                    symbol.address(),
                    symbol.size(),
                    symbol.kind()
                );
                if let Some((offset, section)) = elf_symbol_file_mapping(object, &symbol) {
                    let _ = write!(output, " file {offset} section {section}");
                }
                output.push('\n');
            }
        }
        match_count += 1;
    }
    if match_count == 0 {
        output.push_str("no results\n");
    } else if match_count > INSPECT_INDEX_RESULT_LIMIT {
        let _ = writeln!(
            output,
            "[elf symbols truncated: showing first {INSPECT_INDEX_RESULT_LIMIT} of {match_count} symbols; pass a literal filter]"
        );
    }
}

fn relocation_target_name<'data, O: object::Object<'data>>(
    object: &O,
    relocation: &object::Relocation,
    dynamic: bool,
) -> String {
    match relocation.target() {
        object::RelocationTarget::Symbol(index) if dynamic => object
            .dynamic_symbol_table()
            .and_then(|table| table.symbol_by_index(index).ok())
            .and_then(|symbol| symbol.name().ok().map(str::to_string))
            .unwrap_or_else(|| format!("symbol {}", index.0)),
        object::RelocationTarget::Symbol(index) => object
            .symbol_by_index(index)
            .ok()
            .and_then(|symbol| symbol.name().ok().map(str::to_string))
            .unwrap_or_else(|| format!("symbol {}", index.0)),
        object::RelocationTarget::Section(index) => object
            .section_by_index(index)
            .ok()
            .and_then(|section| section.name().ok().map(str::to_string))
            .unwrap_or_else(|| format!("section {}", index.0)),
        object::RelocationTarget::Absolute => "absolute".to_string(),
        _ => "unknown".to_string(),
    }
}

fn write_elf_relocation<'data, O: object::Object<'data>>(
    object: &O,
    output: &mut String,
    match_count: &mut usize,
    location: &str,
    file_mapping: Option<(u64, String)>,
    relocation: &object::Relocation,
    dynamic: bool,
    literal: Option<&str>,
) {
    let target = relocation_target_name(object, relocation, dynamic);
    let line = format!(
        "{location} target {target} kind {:?} size {} addend {}",
        relocation.kind(),
        relocation.size(),
        relocation.addend()
    );
    if literal.is_some_and(|literal| !line.contains(literal)) {
        return;
    }
    if *match_count < INSPECT_INDEX_RESULT_LIMIT {
        output.push_str(&line);
        if let Some((offset, section)) = file_mapping {
            let _ = write!(output, " file {offset} section {section}");
        }
        output.push('\n');
    }
    *match_count += 1;
}

fn write_elf_relocations<'data, O: object::Object<'data>>(
    object: &O,
    literal: Option<&str>,
    output: &mut String,
) {
    let mut match_count = 0_usize;
    for section in object.sections() {
        let section_name = section.name().unwrap_or("<invalid-name>").to_string();
        for (offset, relocation) in section.relocations() {
            let virtual_address = section.address() + offset;
            let location = format!("section {section_name}+{offset} virtual {virtual_address}");
            let file_mapping = section.file_range().and_then(|(file_offset, file_size)| {
                (offset < file_size).then(|| (file_offset + offset, section_name.clone()))
            });
            write_elf_relocation(
                object,
                output,
                &mut match_count,
                &location,
                file_mapping,
                &relocation,
                false,
                literal,
            );
        }
    }
    if let Some(relocations) = object.dynamic_relocations() {
        for (address, relocation) in relocations {
            write_elf_relocation(
                object,
                output,
                &mut match_count,
                &format!("virtual {address}"),
                elf_virtual_to_file(object, address),
                &relocation,
                true,
                literal,
            );
        }
    }
    if match_count == 0 {
        output.push_str("no results\n");
    } else if match_count > INSPECT_INDEX_RESULT_LIMIT {
        let _ = writeln!(
            output,
            "[elf relocations truncated: showing first {INSPECT_INDEX_RESULT_LIMIT} of {match_count} relocations; pass a literal filter]"
        );
    }
}

fn write_filtered_elf_dynamic_line(
    output: &mut String,
    match_count: &mut usize,
    literal: Option<&str>,
    line: &str,
) {
    if literal.is_some_and(|literal| !line.contains(literal)) {
        return;
    }
    if *match_count < INSPECT_INDEX_RESULT_LIMIT {
        let _ = writeln!(output, "{line}");
    }
    *match_count += 1;
}

fn write_elf_dynamic_tags<'data, Elf, R>(
    elf: &object::read::elf::ElfFile<'data, Elf, R>,
    literal: Option<&str>,
    output: &mut String,
    match_count: &mut usize,
) -> Result<(), String>
where
    Elf: object::read::elf::FileHeader,
    R: object::ReadRef<'data>,
{
    let endian = elf.endian();
    let sections = elf.elf_section_table();
    let Some((dynamic, strings_index)) = sections
        .dynamic(endian, elf.data())
        .map_err(|error| format!("failed to read ELF dynamic table: {error}"))?
    else {
        return Ok(());
    };
    let strings = sections
        .strings(endian, elf.data(), strings_index)
        .map_err(|error| format!("failed to read ELF dynamic strings: {error}"))?;
    for entry in dynamic {
        let label = match entry.tag32(endian) {
            Some(object::elf::DT_NEEDED) => "needed",
            Some(object::elf::DT_SONAME) => "soname",
            Some(object::elf::DT_RPATH) => "rpath",
            Some(object::elf::DT_RUNPATH) => "runpath",
            _ => continue,
        };
        let offset = entry
            .val32(endian)
            .ok_or_else(|| "ELF dynamic string offset does not fit u32".to_string())?;
        let value = strings
            .get(offset)
            .map_err(|_| "failed to read ELF dynamic string".to_string())?;
        let line = format!("{label} {}", String::from_utf8_lossy(value));
        write_filtered_elf_dynamic_line(output, match_count, literal, &line);
    }
    Ok(())
}

fn elf_interpreter<'data, Elf, R>(
    elf: &object::read::elf::ElfFile<'data, Elf, R>,
) -> Result<Option<String>, String>
where
    Elf: object::read::elf::FileHeader,
    R: object::ReadRef<'data>,
{
    for header in elf.elf_program_headers() {
        let Some(bytes) = header
            .interpreter(elf.endian(), elf.data())
            .map_err(|error| format!("failed to read ELF interpreter: {error}"))?
        else {
            continue;
        };
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        return Ok(Some(String::from_utf8_lossy(bytes).into_owned()));
    }
    Ok(None)
}

fn write_elf_dynamic<'data>(
    object: &object::File<'data, &'data object::read::ReadCache<fs::File>>,
    literal: Option<&str>,
    output: &mut String,
) -> Result<(), String> {
    let mut match_count = 0_usize;
    match object {
        object::File::Elf32(elf) => {
            write_elf_dynamic_tags(elf, literal, output, &mut match_count)?;
        }
        object::File::Elf64(elf) => {
            write_elf_dynamic_tags(elf, literal, output, &mut match_count)?;
        }
        _ => unreachable!(),
    }
    let imports = object
        .imports()
        .map_err(|error| format!("failed to read ELF imports: {error}"))?;
    for import in imports {
        let library = String::from_utf8_lossy(import.library());
        let name = String::from_utf8_lossy(import.name());
        let line = if library.is_empty() {
            format!("import {name}")
        } else {
            format!("import {library} {name}")
        };
        write_filtered_elf_dynamic_line(output, &mut match_count, literal, &line);
    }
    for symbol in object.dynamic_symbols() {
        if symbol.is_undefined() {
            continue;
        }
        let Ok(name) = symbol.name() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let line = format!(
            "dynamic-symbol {name} virtual {}+{}",
            symbol.address(),
            symbol.size()
        );
        write_filtered_elf_dynamic_line(output, &mut match_count, literal, &line);
    }
    if match_count == 0 {
        output.push_str("no results\n");
    } else if match_count > INSPECT_INDEX_RESULT_LIMIT {
        let _ = writeln!(
            output,
            "[elf dynamic output truncated: showing first {INSPECT_INDEX_RESULT_LIMIT} of {match_count} entries; pass a literal filter]"
        );
    }
    Ok(())
}

const ELF_INSPECT_USAGE: &str = "failed to parse `inspect` input: usage: `elf <path> [summary|sections|segments|symbols [literal]|relocations [literal]|dynamic [literal]|address <virtual>|offset <file>]`";

fn execute_elf_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let words = match parse_shell_words(command) {
        Ok(words) => words,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` elf input: {message}"
            )));
        }
    };
    if words.len() < 2 {
        return inspect_command_failed(NativeToolExecutionOutput::same(
            ELF_INSPECT_USAGE.to_string(),
        ));
    }
    let path = &words[1].value;
    let query = words
        .get(2)
        .map(|word| word.value.as_str())
        .unwrap_or("summary");
    let argument = words.get(3).map(|word| word.value.as_str());
    if words.len() > 4
        || !matches!(
            (query, argument),
            ("summary" | "sections" | "segments", None)
                | ("symbols" | "relocations" | "dynamic", None | Some(_))
                | ("address" | "offset", Some(_))
        )
    {
        return inspect_command_failed(NativeToolExecutionOutput::same(
            ELF_INSPECT_USAGE.to_string(),
        ));
    }

    let resolved = resolve_root_relative_path(cwd, path);
    let file = match fs::File::open(&resolved) {
        Ok(file) => file,
        Err(error) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to read {}: {error}",
                resolved.display()
            )));
        }
    };
    let cache = object::read::ReadCache::new(file);
    match object::FileKind::parse(&cache) {
        Ok(object::FileKind::Elf32 | object::FileKind::Elf64) => {}
        Ok(kind) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to inspect {}: expected ELF, found {kind:?}",
                resolved.display()
            )));
        }
        Err(error) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to inspect {}: {error}",
                resolved.display()
            )));
        }
    }
    let object = match object::File::parse(&cache) {
        Ok(object) => object,
        Err(error) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse ELF {}: {error}",
                resolved.display()
            )));
        }
    };

    let mut output = String::new();
    let result = match query {
        "summary" => {
            let _ = writeln!(
                output,
                "ELF{} {:?} {}-endian {:?}",
                if object.is_64() { 64 } else { 32 },
                object.architecture(),
                if object.is_little_endian() {
                    "little"
                } else {
                    "big"
                },
                object.kind()
            );
            if object.kind() == object::ObjectKind::Relocatable {
                output.push_str("entry none\n");
            } else {
                let _ = writeln!(output, "entry virtual {}", object.entry());
                if let Some((offset, section)) = elf_virtual_to_file(&object, object.entry()) {
                    let _ = writeln!(output, "entry file {offset} section {section}");
                }
            }
            let interpreter = match &object {
                object::File::Elf32(elf) => elf_interpreter(elf),
                object::File::Elf64(elf) => elf_interpreter(elf),
                _ => unreachable!(),
            };
            match interpreter {
                Ok(Some(interpreter)) => {
                    let _ = writeln!(output, "interpreter {interpreter}");
                }
                Ok(None) => {}
                Err(message) => {
                    return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                        "failed to inspect ELF: {message}"
                    )));
                }
            }
            Ok(())
        }
        "sections" => {
            write_elf_sections(&object, &mut output);
            Ok(())
        }
        "segments" => {
            write_elf_segments(&object, &mut output);
            Ok(())
        }
        "symbols" => {
            write_elf_symbols(&object, argument, &mut output);
            Ok(())
        }
        "relocations" => {
            write_elf_relocations(&object, argument, &mut output);
            Ok(())
        }
        "dynamic" => write_elf_dynamic(&object, argument, &mut output),
        "address" => parse_non_negative_u64(argument.unwrap(), "virtual address").map(|address| {
            let _ = writeln!(output, "virtual {address}");
            match elf_virtual_to_file(&object, address) {
                Some((offset, section)) => {
                    let _ = writeln!(output, "file {offset}");
                    let _ = writeln!(output, "section {section}");
                }
                None => output.push_str("no file-backed range\n"),
            }
        }),
        "offset" => parse_non_negative_u64(argument.unwrap(), "file offset").map(|offset| {
            let _ = writeln!(output, "file {offset}");
            match elf_file_to_virtual(&object, offset) {
                Some((address, section)) => {
                    let _ = writeln!(output, "virtual {address}");
                    let _ = writeln!(output, "section {section}");
                }
                None => output.push_str("no virtual mapping\n"),
            }
        }),
        _ => unreachable!(),
    };
    match result {
        Ok(()) => inspect_command_succeeded(NativeToolExecutionOutput::same(output)),
        Err(message) => inspect_command_failed(NativeToolExecutionOutput::same(format!(
            "failed to inspect ELF: {message}"
        ))),
    }
}

fn execute_fff_command(cwd: &str, command: &FffCommand) -> NativeToolExecutionOutput {
    let output = match command {
        FffCommand::Search(request) => execute_fff_search_to_string(cwd, request),
        FffCommand::Files(request) => execute_fff_files_to_string(cwd, request),
    };
    let output = cap_fff_model_output(output);
    let display_output = colorize_fff_display_output(&output);
    NativeToolExecutionOutput::split(output, display_output)
}

fn execute_fff_search_to_string(cwd: &str, request: &FffSearchRequest) -> String {
    if pattern_matches_everything(&request.pattern, request.mode) {
        return format!(
            "Pattern '{}' matches everything — search needs a concrete substring or identifier. Example: `search MyClass src` or `search \"export function\" src`.",
            request.pattern
        );
    }
    if !request.search_roots.is_empty() {
        let mut matches = Vec::new();
        for root in &request.search_roots {
            collect_fff_search_matches(&root.path, &root.path, &root.label, request, &mut matches);
        }
        return format_fff_search_matches(matches);
    }

    let mut picker = match FilePicker::new(FilePickerOptions {
        base_path: cwd.to_string(),
        enable_content_indexing: true,
        mode: FFFMode::Ai,
        watch: false,
        ..Default::default()
    }) {
        Ok(picker) => picker,
        Err(error) => {
            return format!("failed to initialize fff: {error}");
        }
    };
    if let Err(error) = picker.collect_files() {
        return format!("failed to index files for fff: {error}");
    }

    let query_text = format_fff_query(request);
    let parser = QueryParser::new(fff_search::GrepConfig);
    let query = parser.parse(&query_text);
    let options = GrepSearchOptions {
        max_matches_per_file: 50,
        smart_case: request.smart_case,
        page_limit: 10_000,
        mode: request.mode,
        classify_definitions: true,
        ..Default::default()
    };
    let mut result = picker.grep(&query, &options);
    let mut fuzzy_notice = None;
    if result.matches.is_empty() && request.mode != GrepMode::Regex {
        let fuzzy_query = parser.parse(&query_text);
        let fuzzy_result = picker.grep(
            &fuzzy_query,
            &GrepSearchOptions {
                mode: GrepMode::Fuzzy,
                max_matches_per_file: 50,
                smart_case: request.smart_case,
                page_limit: 10,
                classify_definitions: true,
                ..Default::default()
            },
        );
        if !fuzzy_result.matches.is_empty() {
            result = fuzzy_result;
            fuzzy_notice = Some("0 exact matches. Approximate matches:");
        }
    }

    let mut output = String::new();
    if let Some(error) = &result.regex_fallback_error {
        let _ = writeln!(output, "Invalid regex: {error}; used literal match");
    }
    if let Some(notice) = fuzzy_notice {
        let _ = writeln!(output, "{notice}");
    }
    let mut current_path = String::new();
    let mut displayed_matches = 0usize;
    let mut filtered_matches = 0usize;
    for grep_match in &result.matches {
        let file = result.files[grep_match.file_index];
        let path = file.relative_path(&picker);
        if !matches_fff_path_filters(&path, &request.exact_path_filters)
            || matches_fff_excludes(&path, &request.excludes)
        {
            continue;
        }
        filtered_matches += 1;
        if displayed_matches >= FFF_TOTAL_MATCH_LIMIT {
            continue;
        }
        if path != current_path {
            if !current_path.is_empty() {
                output.push('\n');
            }
            let _ = writeln!(output, "{path}");
            current_path = path;
        }
        let _ = writeln!(
            output,
            "{} {}",
            grep_match.line_number, grep_match.line_content
        );
        displayed_matches += 1;
    }
    if filtered_matches > displayed_matches {
        let _ = writeln!(
            output,
            "\n[fff output truncated: showing first {displayed_matches} of {filtered_matches} matches; refine the query or path constraint]"
        );
    }

    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FffTextMatch {
    path: String,
    line_number: usize,
    line: String,
}

fn collect_fff_search_matches(
    root: &Path,
    directory: &Path,
    label_prefix: &str,
    request: &FffSearchRequest,
    matches: &mut Vec<FffTextMatch>,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if !is_fff_ignored_directory(&path) {
                collect_fff_search_matches(root, &path, label_prefix, request, matches);
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        collect_fff_file_matches(root, &path, label_prefix, request, matches);
    }
}

fn collect_fff_file_matches(
    root: &Path,
    path: &Path,
    label_prefix: &str,
    request: &FffSearchRequest,
    matches: &mut Vec<FffTextMatch>,
) {
    let display_path = labeled_fff_path(root, path, label_prefix);
    if !request
        .query_constraints
        .iter()
        .all(|constraint| matches_fff_path_constraint(&display_path, constraint))
        || !matches_fff_path_filters(&display_path, &request.exact_path_filters)
        || matches_fff_excludes(&display_path, &request.excludes)
    {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let matcher = FffOutputMatcher {
        pattern: &request.pattern,
        mode: request.mode,
        smart_case: request.smart_case,
    };
    for (index, line) in text.lines().enumerate() {
        if matcher.matches(line) {
            matches.push(FffTextMatch {
                path: display_path.clone(),
                line_number: index + 1,
                line: line.to_string(),
            });
        }
    }
}

fn format_fff_search_matches(matches: Vec<FffTextMatch>) -> String {
    let mut output = String::new();
    let mut current_path = String::new();
    let filtered_matches = matches.len();
    let mut displayed_matches = 0usize;
    for grep_match in matches.iter().take(FFF_TOTAL_MATCH_LIMIT) {
        if grep_match.path != current_path {
            if !current_path.is_empty() {
                output.push('\n');
            }
            let _ = writeln!(output, "{}", grep_match.path);
            current_path = grep_match.path.clone();
        }
        let _ = writeln!(output, "{} {}", grep_match.line_number, grep_match.line);
        displayed_matches += 1;
    }
    if filtered_matches > displayed_matches {
        let _ = writeln!(
            output,
            "\n[fff output truncated: showing first {displayed_matches} of {filtered_matches} matches; refine the query or path constraint]"
        );
    }
    output
}

fn execute_fff_files_to_string(cwd: &str, request: &FffFilesRequest) -> String {
    let mut files = Vec::new();
    if request.search_roots.is_empty() {
        collect_fff_files(Path::new(cwd), Path::new(cwd), "", request, &mut files);
    } else {
        for root in &request.search_roots {
            collect_fff_files(&root.path, &root.path, &root.label, request, &mut files);
        }
    }
    files.sort();

    let mut output = String::new();
    let filtered_files = files.len();
    for path in files.iter().take(FFF_TOTAL_MATCH_LIMIT) {
        let _ = writeln!(output, "{path}");
    }
    if filtered_files > FFF_TOTAL_MATCH_LIMIT {
        let _ = writeln!(
            output,
            "\n[fff output truncated: showing first {FFF_TOTAL_MATCH_LIMIT} of {filtered_files} files; refine the path or glob constraint]"
        );
    }
    output
}

fn collect_fff_files(
    root: &Path,
    directory: &Path,
    label_prefix: &str,
    request: &FffFilesRequest,
    files: &mut Vec<String>,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if !is_fff_ignored_directory(&path) {
                collect_fff_files(root, &path, label_prefix, request, files);
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let relative_path = labeled_fff_path(root, &path, label_prefix);
        if matches_fff_file_request(&relative_path, request) {
            files.push(relative_path);
        }
    }
}

fn is_fff_ignored_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, ".git" | "target"))
}

fn matches_fff_file_request(path: &str, request: &FffFilesRequest) -> bool {
    request
        .query_constraints
        .iter()
        .all(|constraint| matches_fff_path_constraint(path, constraint))
        && matches_fff_path_filters(path, &request.exact_path_filters)
        && !matches_fff_excludes(path, &request.excludes)
}

fn format_fff_query(request: &FffSearchRequest) -> String {
    let mut parts = request.query_constraints.clone();
    parts.push(request.pattern.clone());
    parts.join(" ")
}

fn normalize_fff_query_constraint(path: &str) -> Option<String> {
    let mut trimmed = path.trim().trim_start_matches("./").to_string();
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }
    if trimmed.ends_with("/**") {
        trimmed.truncate(trimmed.len() - 2);
    } else if trimmed.ends_with("/**/*") {
        trimmed.truncate(trimmed.len() - 4);
    }
    if trimmed.starts_with('/') || trimmed.ends_with('/') || contains_fff_glob_syntax(&trimmed) {
        return Some(trimmed);
    }
    trimmed.push('/');
    Some(trimmed)
}

fn classify_fff_path_constraint(cwd: &str, path: &str) -> Option<FffPathConstraint> {
    let trimmed = path.trim().trim_start_matches("./");
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }
    if contains_fff_glob_syntax(trimmed) {
        return Some(FffPathConstraint::Query(trimmed.to_string()));
    }

    let path_buf = Path::new(trimmed);
    let absolute_path = if path_buf.is_absolute() {
        path_buf.to_path_buf()
    } else {
        Path::new(cwd).join(path_buf)
    };
    if absolute_path.is_file() {
        let relative_file = normalize_fff_exact_path_filter(cwd, &absolute_path);
        let parent_constraint = Path::new(&relative_file)
            .parent()
            .and_then(Path::to_str)
            .and_then(normalize_fff_query_constraint);
        return Some(FffPathConstraint::ExactFile {
            query_constraint: parent_constraint,
            exact_path: relative_file,
        });
    }
    if absolute_path.is_dir() && !is_project_relative_path(cwd, &absolute_path) {
        return Some(FffPathConstraint::SearchRoot(fff_search_root(
            cwd,
            &absolute_path,
        )));
    }

    normalize_fff_query_constraint(trimmed).map(FffPathConstraint::Query)
}

fn normalize_fff_exact_path_filter(cwd: &str, path: &Path) -> String {
    let path = path.strip_prefix(cwd).unwrap_or(path);
    path.to_string_lossy().trim_start_matches("./").to_string()
}
fn labeled_fff_path(root: &Path, path: &Path, label_prefix: &str) -> String {
    if label_prefix.is_empty() {
        return normalize_fff_exact_path_filter(root.to_str().unwrap_or_default(), path);
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    let relative = relative
        .to_string_lossy()
        .trim_start_matches("./")
        .to_string();
    if relative.is_empty() {
        label_prefix.to_string()
    } else {
        format!("{label_prefix}/{relative}")
    }
}

fn is_project_relative_path(cwd: &str, path: &Path) -> bool {
    path.starts_with(cwd)
}

fn fff_search_root(cwd: &str, path: &Path) -> FffSearchRoot {
    FffSearchRoot {
        path: path.to_path_buf(),
        label: normalize_fff_exact_path_filter(cwd, path),
    }
}

fn matches_fff_path_filters(path: &str, filters: &[String]) -> bool {
    filters.is_empty()
        || filters
            .iter()
            .any(|filter| filter == path || matches_fff_glob_constraint(path, filter))
}

/// Return true when a path matches any exclude glob or prefix constraint.
fn matches_fff_excludes(path: &str, excludes: &[String]) -> bool {
    excludes
        .iter()
        .any(|exclude| matches_fff_path_constraint(path, exclude))
}
fn matches_fff_path_constraint(path: &str, constraint: &str) -> bool {
    let normalized = constraint.trim().trim_start_matches("./");
    if normalized.is_empty() || normalized == "." {
        return true;
    }
    if contains_fff_glob_syntax(normalized) {
        return matches_fff_glob_constraint(path, normalized);
    }
    path.starts_with(normalized)
}

fn matches_fff_glob_constraint(path: &str, glob: &str) -> bool {
    let glob = glob.trim().trim_start_matches("./");
    let path = path.trim_start_matches("./");
    if glob.is_empty() || glob == "." {
        return true;
    }

    matches_fff_wildcard(path.as_bytes(), glob.as_bytes())
}

fn matches_fff_wildcard(path: &[u8], pattern: &[u8]) -> bool {
    let mut path_index = 0usize;
    let mut pattern_index = 0usize;
    let mut last_star = None;
    let mut last_match = 0usize;

    while path_index < path.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == path[path_index])
        {
            path_index += 1;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            last_star = Some(pattern_index);
            last_match = path_index;
            pattern_index += 1;
        } else if let Some(star_index) = last_star {
            pattern_index = star_index + 1;
            last_match += 1;
            path_index = last_match;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FffPathConstraint {
    Query(String),
    ExactFile {
        query_constraint: Option<String>,
        exact_path: String,
    },
    SearchRoot(FffSearchRoot),
}

fn contains_fff_glob_syntax(value: &str) -> bool {
    value
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'{'))
}

fn pattern_matches_everything(pattern: &str, mode: GrepMode) -> bool {
    let trimmed = pattern.trim();
    trimmed.is_empty()
        || matches!(mode, GrepMode::Regex)
            && matches!(
                trimmed,
                ".*" | ".+" | "[\\s\\S]*" | "[\\d\\D]*" | "[\\w\\W]*"
            )
}

fn parse_fff_command(cwd: &str, argv: &[ShellWord]) -> Result<FffCommand, String> {
    let files_positions = argv
        .iter()
        .take_while(|word| word.value != "--")
        .enumerate()
        .filter_map(|(index, word)| (!word.quoted && word.value == "--files").then_some(index))
        .collect::<Vec<_>>();
    match files_positions.as_slice() {
        [] => parse_fff_search_request(cwd, argv).map(FffCommand::Search),
        [files_index] => {
            let mut files_argv = argv.to_vec();
            files_argv.remove(*files_index);
            parse_fff_files_request(cwd, &files_argv).map(FffCommand::Files)
        }
        _ => Err("option `--files` may only be specified once".to_string()),
    }
}

fn parse_fff_files_request(cwd: &str, argv: &[ShellWord]) -> Result<FffFilesRequest, String> {
    let mut query_constraints = Vec::new();
    let mut exact_path_filters = Vec::new();
    let mut excludes = Vec::new();
    let mut search_roots = Vec::new();
    let mut positional_only = false;
    let mut index = 0usize;

    while index < argv.len() {
        let word = &argv[index];
        if positional_only {
            push_fff_file_positional(
                &word.value,
                cwd,
                &mut query_constraints,
                &mut exact_path_filters,
                &mut search_roots,
            );
            index += 1;
            continue;
        }

        if word.value == "--" {
            positional_only = true;
            index += 1;
            continue;
        }

        match word.value.as_str() {
            "--color" => {
                index += 2;
            }
            "-g" | "--glob" => {
                let value = argv
                    .get(index + 1)
                    .ok_or_else(|| format!("option `{}` requires a glob", word.value))?;
                query_constraints.push(value.value.clone());
                index += 2;
            }
            _ if word.value.starts_with("--glob=") => {
                query_constraints.push(word.value["--glob=".len()..].to_string());
                index += 1;
            }
            "--exclude" => {
                let value = argv
                    .get(index + 1)
                    .ok_or_else(|| format!("option `{}` requires a glob", word.value))?;
                excludes.push(value.value.clone());
                index += 2;
            }
            _ if word.value.starts_with("--exclude=") => {
                excludes.push(word.value["--exclude=".len()..].to_string());
                index += 1;
            }
            _ if word.value.starts_with('-') && !word.quoted => {
                return Err(format!("unsupported option `{}`", word.value));
            }
            _ => {
                push_fff_file_positional(
                    &word.value,
                    cwd,
                    &mut query_constraints,
                    &mut exact_path_filters,
                    &mut search_roots,
                );
                index += 1;
            }
        }
    }

    Ok(FffFilesRequest {
        query_constraints,
        exact_path_filters,
        excludes,
        search_roots,
    })
}

fn parse_fff_search_request(cwd: &str, argv: &[ShellWord]) -> Result<FffSearchRequest, String> {
    let mut pattern = None;
    let mut query_constraints = Vec::new();
    let mut exact_path_filters = Vec::new();
    let mut excludes = Vec::new();
    let mut search_roots = Vec::new();
    let mut mode = None;
    let mut smart_case = true;
    let mut positional_only = false;
    let mut index = 0usize;

    while index < argv.len() {
        let word = &argv[index];
        if positional_only {
            push_fff_positional(
                &word.value,
                cwd,
                &mut pattern,
                &mut query_constraints,
                &mut exact_path_filters,
                &mut search_roots,
            );
            index += 1;
            continue;
        }

        if word.value == "--" {
            positional_only = true;
            index += 1;
            continue;
        }

        if word.quoted {
            push_fff_positional(
                &word.value,
                cwd,
                &mut pattern,
                &mut query_constraints,
                &mut exact_path_filters,
                &mut search_roots,
            );
            index += 1;
            continue;
        }

        match word.value.as_str() {
            "--color" => {
                index += 2;
            }
            "-i" | "--ignore-case" => {
                smart_case = false;
                index += 1;
            }
            "-s" | "--case-sensitive" => {
                smart_case = false;
                index += 1;
            }
            "-F" | "--fixed-strings" | "--fixed-string" => {
                mode = Some(GrepMode::PlainText);
                index += 1;
            }
            "-e" | "--regexp" => {
                let value = argv
                    .get(index + 1)
                    .ok_or_else(|| format!("option `{}` requires a pattern", word.value))?;
                pattern = Some(value.value.clone());
                index += 2;
            }
            "-g" | "--glob" => {
                let value = argv
                    .get(index + 1)
                    .ok_or_else(|| format!("option `{}` requires a glob", word.value))?;
                query_constraints.push(value.value.clone());
                index += 2;
            }
            _ if word.value.starts_with("--glob=") => {
                query_constraints.push(word.value["--glob=".len()..].to_string());
                index += 1;
            }
            "--exclude" => {
                let value = argv
                    .get(index + 1)
                    .ok_or_else(|| format!("option `{}` requires a glob", word.value))?;
                excludes.push(value.value.clone());
                index += 2;
            }
            _ if word.value.starts_with("--exclude=") => {
                excludes.push(word.value["--exclude=".len()..].to_string());
                index += 1;
            }
            _ if word.value.starts_with('-') => {
                return Err(format!("unsupported option `{}`", word.value));
            }
            _ => {
                push_fff_positional(
                    &word.value,
                    cwd,
                    &mut pattern,
                    &mut query_constraints,
                    &mut exact_path_filters,
                    &mut search_roots,
                );
                index += 1;
            }
        }
    }

    let pattern = pattern.ok_or_else(|| "expected a search pattern".to_string())?;
    let mode = mode.unwrap_or_else(|| {
        if fff_search::has_regex_metacharacters(&pattern) {
            GrepMode::Regex
        } else {
            GrepMode::PlainText
        }
    });
    Ok(FffSearchRequest {
        pattern,
        query_constraints,
        exact_path_filters,
        excludes,
        search_roots,
        mode,
        smart_case,
    })
}
fn push_fff_positional(
    arg: &str,
    cwd: &str,
    pattern: &mut Option<String>,
    query_constraints: &mut Vec<String>,
    exact_path_filters: &mut Vec<String>,
    search_roots: &mut Vec<FffSearchRoot>,
) {
    if pattern.is_none() {
        *pattern = Some(arg.to_string());
        return;
    }
    match classify_fff_path_constraint(cwd, arg) {
        Some(FffPathConstraint::Query(constraint)) => query_constraints.push(constraint),
        Some(FffPathConstraint::ExactFile {
            query_constraint,
            exact_path,
        }) => {
            if let Some(query_constraint) = query_constraint {
                query_constraints.push(query_constraint);
            }
            exact_path_filters.push(exact_path);
        }
        Some(FffPathConstraint::SearchRoot(root)) => search_roots.push(root),
        None => {}
    }
}
fn push_fff_file_positional(
    arg: &str,
    cwd: &str,
    query_constraints: &mut Vec<String>,
    exact_path_filters: &mut Vec<String>,
    search_roots: &mut Vec<FffSearchRoot>,
) {
    match classify_fff_path_constraint(cwd, arg) {
        Some(FffPathConstraint::Query(constraint)) => query_constraints.push(constraint),
        Some(FffPathConstraint::ExactFile {
            query_constraint,
            exact_path,
        }) => {
            if let Some(query_constraint) = query_constraint {
                query_constraints.push(query_constraint);
            }
            exact_path_filters.push(exact_path);
        }
        Some(FffPathConstraint::SearchRoot(root)) => search_roots.push(root),
        None => {}
    }
}

fn parse_fff_pipeline(cwd: &str, argv: &[ShellWord]) -> Result<Vec<FffPipelineStage>, String> {
    let parts = split_fff_pipeline(argv)?;
    let mut stages = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        let Some(command) = part.first() else {
            return Err("empty pipeline stage".to_string());
        };
        let stage = match command.value.as_str() {
            "fff" | "search" => FffPipelineStage::Fff(parse_fff_command(cwd, &part[1..])?),
            "head" => FffPipelineStage::Head(parse_fff_line_count("head", &part[1..])?),
            "tail" => FffPipelineStage::Tail(parse_fff_line_count("tail", &part[1..])?),
            _ => {
                if index == 0 {
                    return Err("command must start with search".to_string());
                }
                return Err(format!(
                    "unsupported search pipeline stage `{}`; supported stages are `search`, `head`, and `tail`",
                    command.value
                ));
            }
        };
        stages.push(stage);
    }
    Ok(stages)
}

fn split_fff_pipeline(argv: &[ShellWord]) -> Result<Vec<Vec<ShellWord>>, String> {
    let mut parts = Vec::new();
    let mut current = Vec::new();
    for word in argv {
        if !word.quoted && word.value == "|" {
            if current.is_empty() {
                return Err("empty pipeline stage".to_string());
            }
            parts.push(current);
            current = Vec::new();
        } else if !word.quoted && shell_operator_arg(&word.value) {
            return Err(format!("shell operator `{}` is not supported", word.value));
        } else {
            current.push(word.clone());
        }
    }
    if current.is_empty() {
        return Err("empty pipeline stage".to_string());
    }
    parts.push(current);
    Ok(parts)
}

fn parse_fff_line_count(command: &str, argv: &[ShellWord]) -> Result<usize, String> {
    match argv {
        [] => Ok(10),
        [count] if !count.quoted => parse_positive_usize(command, &count.value),
        [option, count] if !option.quoted && !count.quoted && option.value == "-n" => {
            parse_positive_usize(command, &count.value)
        }
        _ => Err(format!("unsupported `{command}` arguments")),
    }
}

fn parse_positive_usize(command: &str, value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("`{command}` count must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("`{command}` count must be a positive integer"));
    }
    Ok(parsed)
}

fn execute_fff_pipeline(cwd: &str, stages: &[FffPipelineStage]) -> NativeToolExecutionOutput {
    if stages.len() == 1
        && let FffPipelineStage::Fff(command) = &stages[0]
    {
        return execute_fff_command(cwd, command);
    }

    let mut output = String::new();
    for (index, stage) in stages.iter().enumerate() {
        match stage {
            FffPipelineStage::Fff(command) if index == 0 => {
                output = match command {
                    FffCommand::Search(request) => execute_fff_search_to_string(cwd, request),
                    FffCommand::Files(request) => execute_fff_files_to_string(cwd, request),
                };
            }
            FffPipelineStage::Fff(FffCommand::Search(request)) => {
                output = filter_fff_output(&output, request);
            }
            FffPipelineStage::Fff(FffCommand::Files(_)) => {
                return NativeToolExecutionOutput::same(
                    "failed to parse `fff` input: `fff --files` is only supported as the first pipeline stage",
                );
            }
            FffPipelineStage::Head(count) => {
                output = take_fff_lines(&output, *count);
            }
            FffPipelineStage::Tail(count) => {
                output = tail_fff_lines(&output, *count);
            }
        }
    }

    let output = cap_fff_model_output(output);
    let display_output = colorize_fff_display_output(&output);
    NativeToolExecutionOutput::split(output, display_output)
}

fn filter_fff_output(input: &str, request: &FffSearchRequest) -> String {
    let matcher = FffOutputMatcher::new(request);
    let mut output = String::new();
    let mut current_path = String::new();
    let mut pending_file_header = false;

    for line in input.lines() {
        if is_fff_notice_line(line) {
            continue;
        }
        if is_fff_match_line(line) {
            if matcher.matches(line) {
                if pending_file_header {
                    let _ = writeln!(output, "{current_path}");
                    pending_file_header = false;
                }
                let _ = writeln!(output, "{line}");
            }
        } else {
            current_path.clear();
            current_path.push_str(line);
            pending_file_header = true;
        }
    }

    output
}

fn is_fff_notice_line(line: &str) -> bool {
    line.starts_with("Invalid regex:")
        || line.starts_with("0 exact matches.")
        || line.starts_with('[')
}

fn is_fff_match_line(line: &str) -> bool {
    line.split_once(' ').is_some_and(|(line_number, _)| {
        line_number
            .chars()
            .all(|character| character.is_ascii_digit())
    })
}

struct FffOutputMatcher<'a> {
    pattern: &'a str,
    mode: GrepMode,
    smart_case: bool,
}

impl<'a> FffOutputMatcher<'a> {
    fn new(request: &'a FffSearchRequest) -> Self {
        Self {
            pattern: &request.pattern,
            mode: request.mode,
            smart_case: request.smart_case,
        }
    }

    fn matches(&self, line: &str) -> bool {
        match self.mode {
            GrepMode::PlainText | GrepMode::Fuzzy => self.matches_plain_text(line),
            GrepMode::Regex => self.matches_regex_subset(line),
        }
    }

    fn matches_plain_text(&self, line: &str) -> bool {
        if self.smart_case && self.pattern.chars().any(char::is_uppercase) {
            line.contains(self.pattern)
        } else {
            line.to_lowercase().contains(&self.pattern.to_lowercase())
        }
    }

    fn matches_regex_subset(&self, line: &str) -> bool {
        self.pattern
            .split('|')
            .any(|part| !part.is_empty() && self.regex_atom_matches(line, part))
    }

    fn regex_atom_matches(&self, line: &str, atom: &str) -> bool {
        let atom = atom.strip_prefix(".*").unwrap_or(atom);
        let atom = atom.strip_suffix(".*").unwrap_or(atom);
        let atom = atom.replace("\\(", "(").replace("\\)", ")");
        if self.smart_case && atom.chars().any(char::is_uppercase) {
            line.contains(&atom)
        } else {
            line.to_lowercase().contains(&atom.to_lowercase())
        }
    }
}

fn take_fff_lines(input: &str, count: usize) -> String {
    let mut output = input.lines().take(count).collect::<Vec<_>>().join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    output
}

fn tail_fff_lines(input: &str, count: usize) -> String {
    let lines = input.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(count);
    let mut output = lines[start..].join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    output
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InspectionDisplayKind {
    Fff,
    ProcessStatus,
}

fn execute_argv_command(
    cwd: &str,
    argv: &[String],
    display_kind: InspectionDisplayKind,
) -> InspectCommandExecution {
    let Some((program, args)) = argv.split_first() else {
        return inspect_command_failed(NativeToolExecutionOutput::same("missing command"));
    };
    let output = match Command::new(program).args(args).current_dir(cwd).output() {
        Ok(output) => output,
        Err(error) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to execute `{}`: {error}",
                argv.join(" ")
            )));
        }
    };
    let succeeded = output.status.success();

    let mut model_output = String::new();
    model_output.push_str(&String::from_utf8_lossy(&output.stdout));
    model_output.push_str(&String::from_utf8_lossy(&output.stderr));
    if model_output.is_empty() && !output.status.success() {
        let _ = writeln!(
            model_output,
            "exit status: {}",
            output_status_text(output.status)
        );
    }
    if display_kind == InspectionDisplayKind::Fff {
        model_output = cap_fff_model_output(model_output);
    }
    let display_output = match display_kind {
        InspectionDisplayKind::Fff => colorize_fff_display_output(&model_output),
        InspectionDisplayKind::ProcessStatus => {
            colorize_process_status_display_output(&model_output)
        }
    };
    InspectCommandExecution {
        output: NativeToolExecutionOutput::split(model_output, display_output),
        succeeded,
    }
}

fn cap_fff_model_output(output: String) -> String {
    cap_model_output_for_estimated_tokens(output, FFF_MODEL_OUTPUT_MAX_TOKENS, "fff")
}

fn cap_model_output_for_estimated_tokens(output: String, max_tokens: u64, label: &str) -> String {
    let original_tokens = estimate_text_tokens(&output);
    if original_tokens <= max_tokens {
        return output;
    }

    let notice = format!(
        "\n\n[{label} output truncated before summarization: original approximately {original_tokens} tokens; retained first {max_tokens} tokens]\n"
    );
    let notice_tokens = estimate_text_tokens(&notice);
    let content_budget = max_tokens.saturating_sub(notice_tokens);
    let prefix = estimated_token_prefix(&output, content_budget);
    format!("{prefix}{notice}")
}

fn estimated_token_prefix(text: &str, max_tokens: u64) -> &str {
    if max_tokens == 0 {
        return "";
    }
    if estimate_text_tokens(text) <= max_tokens {
        return text;
    }

    let boundaries = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let mut low = 0usize;
    let mut high = boundaries.len() - 1;
    while low < high {
        let middle = (low + high).div_ceil(2);
        let candidate = &text[..boundaries[middle]];
        if estimate_text_tokens(candidate) <= max_tokens {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    &text[..boundaries[low]]
}

fn colorize_fff_display_output(output: &str) -> String {
    let mut colorized = String::new();
    for line in output.lines() {
        let _ = writeln!(colorized, "{}", colorize_fff_display_line(line));
    }
    colorized
}

fn colorize_fff_display_line(line: &str) -> String {
    if let Some((line_number, text)) = line.split_once(' ')
        && line_number
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return format!("{ANSI_GREEN}{line_number}{ANSI_RESET} {text}");
    }

    let mut parts = line.splitn(4, ':');
    let Some(path) = parts.next() else {
        return line.to_string();
    };
    let Some(line_number) = parts.next() else {
        if line.is_empty()
            || line.starts_with("Invalid regex:")
            || line.starts_with("0 exact matches.")
            || line.starts_with('[')
        {
            return line.to_string();
        }
        return format!("{ANSI_CYAN}{line}{ANSI_RESET}");
    };
    if path.chars().all(|character| character.is_ascii_digit()) {
        return format!("{ANSI_GREEN}{path}{ANSI_RESET}:{line_number}");
    }
    if !line_number
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        return format!("{ANSI_CYAN}{path}{ANSI_RESET}:{line_number}");
    }
    let Some(rest) = parts.next() else {
        return format!("{ANSI_CYAN}{path}{ANSI_RESET}:{ANSI_GREEN}{line_number}{ANSI_RESET}");
    };
    if rest.chars().all(|character| character.is_ascii_digit()) {
        let Some(text) = parts.next() else {
            return format!(
                "{ANSI_CYAN}{path}{ANSI_RESET}:{ANSI_GREEN}{line_number}{ANSI_RESET}:{ANSI_BLUE}{rest}{ANSI_RESET}"
            );
        };
        format!(
            "{ANSI_CYAN}{path}{ANSI_RESET}:{ANSI_GREEN}{line_number}{ANSI_RESET}:{ANSI_BLUE}{rest}{ANSI_RESET}:{text}"
        )
    } else {
        format!("{ANSI_CYAN}{path}{ANSI_RESET}:{ANSI_GREEN}{line_number}{ANSI_RESET}:{rest}")
    }
}

fn colorize_process_status_display_output(output: &str) -> String {
    let mut colorized = String::new();
    for line in output.lines() {
        let trimmed = line.trim_start();
        let leading_spaces_len = line.len() - trimmed.len();
        let leading_spaces = &line[..leading_spaces_len];
        let Some((pid, rest)) = trimmed.split_once(char::is_whitespace) else {
            let _ = writeln!(colorized, "{line}");
            continue;
        };
        if pid.chars().all(|character| character.is_ascii_digit()) {
            let _ = writeln!(
                colorized,
                "{leading_spaces}{ANSI_GREEN}{pid}{ANSI_RESET} {rest}"
            );
        } else {
            let _ = writeln!(colorized, "{line}");
        }
    }
    colorized
}

fn output_status_text(status: std::process::ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_string())
}

fn shell_operator_arg(arg: &str) -> bool {
    matches!(arg, "|" | "||" | "&&" | ";" | ">" | ">>" | "<")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShellWord {
    value: String,
    quoted: bool,
}

fn parse_shell_words(input: &str) -> Result<Vec<ShellWord>, String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut current_quoted = false;
    let mut chars = input.chars().peekable();
    let mut in_word = false;
    while let Some(character) = chars.next() {
        match (quote, character) {
            (Quote::None, '\'') => {
                quote = Quote::Single;
                current_quoted = true;
                in_word = true;
            }
            (Quote::None, '"') => {
                quote = Quote::Double;
                current_quoted = true;
                in_word = true;
            }
            (Quote::Single, '\'') => {
                quote = Quote::None;
            }
            (Quote::Double, '"') => {
                quote = Quote::None;
            }
            (Quote::None, '\\') => {
                let Some(escaped) = chars.next() else {
                    return Err("trailing backslash".to_string());
                };
                current.push(escaped);
                in_word = true;
            }
            (Quote::Double, '\\') => {
                let Some(next) = chars.next() else {
                    return Err("trailing backslash".to_string());
                };
                // Inside double quotes, backslash is only special before the
                // POSIX-defined set of characters. For anything else (e.g. \[)
                // both the backslash and the character are preserved literally,
                // so that regex tools like search receive \[ rather than a bare [.
                if matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                    current.push(next);
                } else {
                    current.push('\\');
                    current.push(next);
                }
                in_word = true;
            }
            (Quote::None, character) if character.is_whitespace() => {
                if in_word {
                    words.push(ShellWord {
                        value: std::mem::take(&mut current),
                        quoted: current_quoted,
                    });
                    in_word = false;
                    current_quoted = false;
                }
            }
            (_, character) => {
                current.push(character);
                in_word = true;
            }
        }
    }

    match quote {
        Quote::None => {}
        Quote::Single => return Err("unterminated single quote".to_string()),
        Quote::Double => return Err("unterminated double quote".to_string()),
    }
    if in_word {
        words.push(ShellWord {
            value: current,
            quoted: current_quoted,
        });
    }
    Ok(words)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InspectChainOperator {
    Always,
    OnSuccess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InspectChainedCommand {
    operator: InspectChainOperator,
    command: String,
}

async fn execute_inspect_tool(cwd: String, input: &str) -> NativeToolExecutionOutput {
    let request = match parse_inspect_input(input) {
        Ok(request) => request,
        Err(message) => return NativeToolExecutionOutput::same(message),
    };

    let handles = request
        .jobs
        .into_iter()
        .map(|job| {
            let cwd = cwd.clone();
            tokio::task::spawn_blocking(move || {
                let output = execute_inspect_job(&cwd, &job);
                InspectJobResult { job, output }
            })
        })
        .collect::<Vec<_>>();

    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(error) => {
                return NativeToolExecutionOutput::same(format!(
                    "`{INSPECT_TOOL_NAME}` worker failed: {error}"
                ));
            }
        }
    }

    NativeToolExecutionOutput::split(
        format_inspect_model_output(&results),
        format_inspect_display_output(&results),
    )
    .with_structured_display(format_inspect_structured_display(&results))
}

fn execute_inspect_job(cwd: &str, job: &InspectJob) -> NativeToolExecutionOutput {
    match job {
        InspectJob::Read(read) => execute_inspect_read_job(cwd, read),
        InspectJob::Command(command) => execute_inspect_command_chain(cwd, &command.command),
    }
}

fn execute_inspect_read_job(cwd: &str, job: &InspectReadJob) -> NativeToolExecutionOutput {
    let path = resolve_root_relative_path(cwd, &job.path);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return NativeToolExecutionOutput::same(format!(
                "failed to read {}: {error}",
                path.display()
            ));
        }
    };

    let mut output = String::new();
    let mut display = Vec::new();
    for range in &job.ranges {
        let request = InspectReadOutputRequest {
            path: job.path.clone(),
            start_line: range.start_line,
            line_count: range.line_count,
        };
        output.push_str(&format_inspect_read_output(&request, &text));
        display.push(format_inspect_read_display(&request, &text));
    }
    NativeToolExecutionOutput::split(output, String::new())
        .with_structured_display(Some(ToolOutputDisplayRecord::InspectRead(display)))
}

fn execute_inspect_command_chain(cwd: &str, command: &str) -> NativeToolExecutionOutput {
    let commands = match parse_inspect_command_chain(command) {
        Ok(commands) => commands,
        Err(message) => return NativeToolExecutionOutput::same(message),
    };

    let mut previous_succeeded = true;
    let mut model_output = String::new();
    let mut display_output = String::new();
    for command in commands {
        if command.operator == InspectChainOperator::OnSuccess && !previous_succeeded {
            previous_succeeded = false;
            continue;
        }
        let executed = execute_inspect_command(cwd, &command.command);
        previous_succeeded = executed.succeeded;
        model_output.push_str(&executed.output.model_output);
        if !executed.output.model_output.is_empty() && !executed.output.model_output.ends_with('\n')
        {
            model_output.push('\n');
        }
        display_output.push_str(&executed.output.display_output);
        if !executed.output.display_output.is_empty()
            && !executed.output.display_output.ends_with('\n')
        {
            display_output.push('\n');
        }
    }

    NativeToolExecutionOutput::split(model_output, display_output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectCommandExecution {
    output: NativeToolExecutionOutput,
    succeeded: bool,
}

fn inspect_command_succeeded(output: NativeToolExecutionOutput) -> InspectCommandExecution {
    InspectCommandExecution {
        output,
        succeeded: true,
    }
}

fn inspect_command_failed(output: NativeToolExecutionOutput) -> InspectCommandExecution {
    InspectCommandExecution {
        output,
        succeeded: false,
    }
}

fn execute_inspect_command(cwd: &str, command: &str) -> InspectCommandExecution {
    if command == "pwd" {
        return inspect_command_succeeded(execute_pwd_tool(cwd, "pwd\n"));
    }
    if command == "list" || command.starts_with("list ") {
        return execute_list_command(cwd, command);
    }
    if command == "stat" || command.starts_with("stat ") {
        return execute_stat_command(cwd, command);
    }
    if command == "bytes" || command.starts_with("bytes ") {
        return execute_bytes_command(cwd, command);
    }
    if command == "byte-search" || command.starts_with("byte-search ") {
        return execute_byte_search_command(cwd, command);
    }
    if command == "strings" || command.starts_with("strings ") {
        return execute_strings_command(cwd, command);
    }
    if command == "elf" || command.starts_with("elf ") {
        return execute_elf_command(cwd, command);
    }
    if command.starts_with("search ") || command == "search" {
        return execute_search_command(cwd, command);
    }
    if command == "check" || command.starts_with("check ") {
        return execute_cargo_check_command(cwd, command);
    }
    if command == "test" || command.starts_with("test ") {
        return execute_cargo_test_command(cwd, command);
    }
    if command == "which" || command.starts_with("which ") {
        return execute_path_command_search(cwd, command);
    }
    if command == "ps" || command.starts_with("ps ") {
        return execute_process_status_command(cwd, command);
    }
    inspect_command_failed(NativeToolExecutionOutput::same(format!(
        "failed to parse `inspect` input: unsupported command `{command}`"
    )))
}

fn execute_search_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let input = if let Some(rest) = command.strip_prefix("search ") {
        format!("fff {rest}")
    } else {
        "fff".to_string()
    };
    let argv = match parse_shell_words(&input) {
        Ok(argv) => argv,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` search input: {message}"
            )));
        }
    };
    let stages = match parse_fff_pipeline(cwd, &argv) {
        Ok(stages) => stages,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` search input: {message}"
            )));
        }
    };
    inspect_command_succeeded(execute_fff_pipeline(cwd, &stages))
}

fn execute_path_command_search(cwd: &str, command: &str) -> InspectCommandExecution {
    let words = match parse_shell_words(command) {
        Ok(words) => words,
        Err(message) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to parse `inspect` which input: {message}"
            )));
        }
    };
    let [name, query] = words.as_slice() else {
        return inspect_command_failed(NativeToolExecutionOutput::same(
            "failed to parse `inspect` input: usage: `which <query>`".to_string(),
        ));
    };
    if name.value != "which" || query.value.is_empty() {
        return inspect_command_failed(NativeToolExecutionOutput::same(
            "failed to parse `inspect` input: usage: `which <query>`".to_string(),
        ));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return inspect_command_failed(NativeToolExecutionOutput::same(
            "failed to search commands: PATH is not set".to_string(),
        ));
    };

    let output = format_path_command_matches(
        search_path_commands(Path::new(cwd), &path, &query.value),
        INSPECT_COMMAND_MATCH_LIMIT,
    );
    inspect_command_succeeded(NativeToolExecutionOutput::same(output))
}

fn search_path_commands(cwd: &Path, path: &std::ffi::OsStr, query: &str) -> Vec<PathCommandMatch> {
    let mut commands = Vec::new();
    let mut seen = HashSet::new();
    for directory in std::env::split_paths(path) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        };
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(score) = fuzzy_path_command_score(query, &name) else {
                continue;
            };
            commands.push(PathCommandMatch {
                name,
                path: entry.path(),
                score,
            });
        }
    }
    commands.sort_by(|left, right| {
        left.score
            .cmp(&right.score)
            .then_with(|| left.name.cmp(&right.name))
    });
    if let Some(best_quality) = commands.first().map(|command| command.score.0) {
        commands.retain(|command| command.score.0 == best_quality);
    }
    commands
}

fn fuzzy_path_command_score(query: &str, candidate: &str) -> Option<(u8, usize, usize, usize)> {
    let query = query.to_ascii_lowercase();
    let candidate = candidate.to_ascii_lowercase();
    let candidate_len = candidate.chars().count();

    if candidate == query {
        return Some((0, 0, 0, candidate_len));
    }
    if candidate.starts_with(&query) {
        return Some((1, 0, 0, candidate_len));
    }
    if let Some(start) = candidate.find(&query) {
        return Some((2, 0, start, candidate_len));
    }

    let candidate_chars = candidate.chars().collect::<Vec<_>>();
    let mut next_index = 0usize;
    let mut first_index = None;
    let mut last_index = 0usize;
    let mut query_len = 0usize;
    for query_char in query.chars() {
        let offset = candidate_chars[next_index..]
            .iter()
            .position(|candidate_char| *candidate_char == query_char)?;
        let matched_index = next_index + offset;
        first_index.get_or_insert(matched_index);
        last_index = matched_index;
        next_index = matched_index + 1;
        query_len += 1;
    }
    let first_index = first_index?;
    let span = last_index - first_index + 1;
    Some((3, span - query_len, first_index, candidate_len))
}

fn format_path_command_matches(matches: Vec<PathCommandMatch>, limit: usize) -> String {
    if matches.is_empty() {
        return "no results\n".to_string();
    }

    let total = matches.len();
    let mut output = String::new();
    for command_match in matches.into_iter().take(limit) {
        let _ = writeln!(
            output,
            "{} {}",
            command_match.name,
            command_match.path.display()
        );
    }
    if total > limit {
        let _ = writeln!(output, "[{} more matches]", total - limit);
    }
    output
}

fn execute_process_status_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let mut parts = command.split_whitespace();
    let _ = parts.next();
    let filter = parts.next().map(str::to_string);
    if parts.next().is_some() {
        return inspect_command_failed(NativeToolExecutionOutput::same(
            "failed to parse `inspect` input: usage: `ps [name]`".to_string(),
        ));
    }
    let argv = vec!["ps".to_string(), "aux".to_string()];
    let mut execution = execute_argv_command(cwd, &argv, InspectionDisplayKind::ProcessStatus);
    if let Some(filter) = filter {
        execution = filter_process_status_output(execution, &filter);
    }
    execution
}

fn filter_process_status_output(
    execution: InspectCommandExecution,
    filter: &str,
) -> InspectCommandExecution {
    let filter_lower = filter.to_ascii_lowercase();
    let mut lines = execution.output.model_output.lines();
    let header = lines.next();
    let matching_lines = lines
        .filter(|line| line.to_ascii_lowercase().contains(&filter_lower))
        .collect::<Vec<_>>();
    let filtered = if matching_lines.is_empty() {
        "no results\n".to_string()
    } else {
        let mut filtered = String::new();
        if let Some(header) = header {
            filtered.push_str(header);
            filtered.push('\n');
        }
        for line in matching_lines {
            filtered.push_str(line);
            filtered.push('\n');
        }
        filtered
    };
    let display_output = colorize_process_status_display_output(&filtered);
    InspectCommandExecution {
        output: NativeToolExecutionOutput::split(filtered, display_output),
        succeeded: execution.succeeded,
    }
}

fn parse_inspect_input(input: &str) -> Result<InspectRequest, String> {
    let lines = input.lines().collect::<Vec<_>>();
    if lines.iter().all(|line| line.trim().is_empty()) {
        return Err(
            "failed to parse `inspect` input: at least one command is required".to_string(),
        );
    }

    let mut jobs = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index].trim();
        if line.is_empty() {
            index += 1;
            continue;
        }

        if let Some(read_line) = line.strip_prefix("read ") {
            let read_line = read_line.trim();
            if read_line.is_empty() {
                return Err("failed to parse `inspect` input: read path is required".to_string());
            }
            let (path, mut ranges) = parse_inspect_read_header(read_line)?;
            index += 1;
            while index < lines.len() {
                let range_line = lines[index].trim();
                if range_line.is_empty() {
                    index += 1;
                    continue;
                }
                if inspect_command_start(range_line) {
                    break;
                }
                ranges.push(parse_inspect_read_range(range_line)?);
                index += 1;
            }
            if ranges.is_empty() {
                return Err(format!(
                    "failed to parse `inspect` input: read `{path}` needs at least one range"
                ));
            }
            jobs.push(InspectJob::Read(InspectReadJob { path, ranges }));
            continue;
        }

        if inspect_command_start(line) {
            parse_inspect_command_chain(line)?;
            jobs.push(InspectJob::Command(InspectCommandJob {
                command: line.to_string(),
            }));
            index += 1;
            continue;
        }

        return Err(format!(
            "failed to parse `inspect` input: unknown command line `{line}`"
        ));
    }

    Ok(InspectRequest { jobs })
}

fn inspect_command_start(line: &str) -> bool {
    line == "pwd"
        || line == "list"
        || line == "stat"
        || line == "bytes"
        || line == "byte-search"
        || line == "strings"
        || line == "elf"
        || line == "search"
        || line == "check"
        || line == "which"
        || line == "test"
        || line == "ps"
        || line.starts_with("read ")
        || line.starts_with("list ")
        || line.starts_with("stat ")
        || line.starts_with("bytes ")
        || line.starts_with("byte-search ")
        || line.starts_with("strings ")
        || line.starts_with("elf ")
        || line.starts_with("search ")
        || line.starts_with("which ")
        || line.starts_with("check ")
        || line.starts_with("test ")
        || line.starts_with("ps ")
}

fn parse_inspect_read_header(input: &str) -> Result<(String, Vec<InspectReadRange>), String> {
    let parts = parse_shell_words(input)
        .map_err(|message| format!("failed to parse `inspect` read input: {message}"))?;
    let Some(path) = parts.first() else {
        return Err("failed to parse `inspect` input: read path is required".to_string());
    };
    let mut ranges = Vec::new();
    for part in &parts[1..] {
        ranges.push(parse_inspect_read_range(&part.value)?);
    }
    Ok((path.value.clone(), ranges))
}
fn parse_inspect_read_range(range: &str) -> Result<InspectReadRange, String> {
    if let Some((start, count)) = range.split_once('+') {
        let start_line = parse_positive_usize_value(start).map_err(|_| {
            "failed to parse `inspect` input: range start must be a positive integer".to_string()
        })?;
        let line_count = parse_positive_usize_value(count).map_err(|_| {
            "failed to parse `inspect` input: range count must be a positive integer".to_string()
        })?;
        return Ok(InspectReadRange {
            start_line,
            line_count,
        });
    }

    if let Some((start, end)) = range.split_once('-') {
        let start_line = parse_positive_usize_value(start).map_err(|_| {
            "failed to parse `inspect` input: range start must be a positive integer".to_string()
        })?;
        let end_line = parse_positive_usize_value(end).map_err(|_| {
            "failed to parse `inspect` input: range end must be a positive integer".to_string()
        })?;
        if end_line < start_line {
            return Err("failed to parse `inspect` input: range end must be >= start".to_string());
        }
        return Ok(InspectReadRange {
            start_line,
            line_count: end_line - start_line + 1,
        });
    }

    Err("failed to parse `inspect` input: range must be `start+count` or `start-end`".to_string())
}

fn parse_positive_usize_value(value: &str) -> Result<usize, ()> {
    let parsed = value.trim().parse::<usize>().map_err(|_| ())?;
    if parsed == 0 {
        return Err(());
    }
    Ok(parsed)
}

fn parse_inspect_command_chain(command: &str) -> Result<Vec<InspectChainedCommand>, String> {
    let words = parse_shell_words(command)
        .map_err(|message| format!("failed to parse `inspect` input: {message}"))?;
    let mut commands = Vec::new();
    let mut current = Vec::new();
    let mut next_operator = InspectChainOperator::Always;

    for word in words {
        if !word.quoted && (word.value == "&&" || word.value == ";") {
            if current.is_empty() {
                return Err(format!(
                    "failed to parse `inspect` input: empty command before `{}`",
                    word.value
                ));
            }
            commands.push(InspectChainedCommand {
                operator: next_operator,
                command: shell_words_to_command(&current),
            });
            current.clear();
            next_operator = if word.value == "&&" {
                InspectChainOperator::OnSuccess
            } else {
                InspectChainOperator::Always
            };
        } else {
            current.push(word);
        }
    }

    if current.is_empty() {
        return Err("failed to parse `inspect` input: empty command after operator".to_string());
    }
    commands.push(InspectChainedCommand {
        operator: next_operator,
        command: shell_words_to_command(&current),
    });
    Ok(commands)
}

fn shell_words_to_command(words: &[ShellWord]) -> String {
    words
        .iter()
        .map(shell_word_for_command)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_word_for_command(word: &ShellWord) -> String {
    let value = &word.value;
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || (word.quoted
            && value
                .chars()
                .any(|character| matches!(character, '&' | ';' | '|' | '>' | '<')))
    {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

fn format_inspect_model_output(results: &[InspectJobResult]) -> String {
    let mut output = String::new();
    for result in results {
        output.push_str(&result.output.model_output);
        if !result.output.model_output.is_empty() && !result.output.model_output.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

fn format_inspect_display_output(results: &[InspectJobResult]) -> String {
    let mut output = String::new();
    for result in results {
        if result.output.display_output.is_empty() {
            continue;
        }
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&result.output.display_output);
        if !result.output.display_output.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

fn format_inspect_structured_display(
    results: &[InspectJobResult],
) -> Option<ToolOutputDisplayRecord> {
    let mut reads = Vec::new();
    for result in results {
        if let Some(ToolOutputDisplayRecord::InspectRead(job_reads)) = &result.output.display {
            reads.extend(job_reads.iter().cloned());
        }
    }
    (!reads.is_empty()).then_some(ToolOutputDisplayRecord::InspectRead(reads))
}

fn parse_cargo_check_command(command: &str) -> Result<Vec<String>, String> {
    let words = parse_shell_words(command)
        .map_err(|message| format!("failed to parse `inspect` check input: {message}"))?;
    let Some(first) = words.first() else {
        return Err("failed to parse `inspect` check input: expected check command".to_string());
    };
    if first.value != "check" {
        return Err(
            "failed to parse `inspect` check input: command must start with check".to_string(),
        );
    }

    let mut command_args = vec!["check".to_string(), "--locked".to_string()];
    for word in words.iter().skip(1) {
        match word.value.as_str() {
            "--lib" | "--all-targets" => command_args.push(word.value.clone()),
            package if !package.starts_with('-') && !package.contains('=') => {
                command_args.push("-p".to_string());
                command_args.push(package.to_string());
            }
            _ => {
                return Err(
                    "failed to parse `inspect` check input: expected package names, --lib, or --all-targets"
                        .to_string(),
                );
            }
        }
    }

    Ok(command_args)
}

fn execute_cargo_check_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let args = match parse_cargo_check_command(command) {
        Ok(args) => args,
        Err(message) => return inspect_command_failed(NativeToolExecutionOutput::same(message)),
    };
    let output = match Command::new("cargo").args(&args).current_dir(cwd).output() {
        Ok(output) => output,
        Err(error) => {
            return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                "failed to execute `cargo check`: {error}"
            )));
        }
    };
    let succeeded = output.status.success();
    InspectCommandExecution {
        output: NativeToolExecutionOutput::same(format_cargo_check_output(
            output.status,
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )),
        succeeded,
    }
}

fn format_cargo_check_output(status: ExitStatus, _stdout: &str, stderr: &str) -> String {
    let diagnostics = rust_error_locations(stderr);
    if diagnostics.is_empty() {
        if status.success() {
            return "ok\n".to_string();
        }
        return format!("cargo check failed {}\n", output_status_text(status));
    }

    format_rust_diagnostics(&diagnostics)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoTestCommand {
    cargo_args: Vec<String>,
    filters: Vec<String>,
    libtest_args: Vec<String>,
}

fn parse_cargo_test_command(command: &str) -> Result<CargoTestCommand, String> {
    let words = parse_shell_words(command)
        .map_err(|message| format!("failed to parse `inspect` test input: {message}"))?;
    let Some(first) = words.first() else {
        return Err("failed to parse `inspect` test input: expected test command".to_string());
    };
    if first.value != "test" {
        return Err(
            "failed to parse `inspect` test input: command must start with test".to_string(),
        );
    }

    let mut parsed = CargoTestCommand {
        cargo_args: vec!["test".to_string(), "--locked".to_string()],
        filters: Vec::new(),
        libtest_args: Vec::new(),
    };
    let mut index = 1;
    let mut parsing_libtest_args = false;
    while index < words.len() {
        let value = words[index].value.as_str();
        if parsing_libtest_args {
            match value {
                "--exact" | "--ignored" | "--include-ignored" | "--show-output" | "--nocapture" => {
                    parsed.libtest_args.push(value.to_string());
                }
                "--skip" | "--test-threads" => {
                    parsed.libtest_args.push(value.to_string());
                    index += 1;
                    let argument = words.get(index).ok_or_else(|| {
                        format!(
                            "failed to parse `inspect` test input: {value} requires an argument"
                        )
                    })?;
                    parsed.libtest_args.push(argument.value.clone());
                }
                value if value.starts_with("--skip=") || value.starts_with("--test-threads=") => {
                    parsed.libtest_args.push(value.to_string());
                }
                _ => {
                    return Err(format!(
                        "failed to parse `inspect` test input: unsupported libtest option `{value}`"
                    ));
                }
            }
            index += 1;
            continue;
        }

        match value {
            "--" => parsing_libtest_args = true,
            "--lib"
            | "--bins"
            | "--examples"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--doc"
            | "--workspace"
            | "--all"
            | "--all-features"
            | "--no-default-features"
            | "--release"
            | "--no-fail-fast" => parsed.cargo_args.push(value.to_string()),
            "-p" | "--package" | "--bin" | "--example" | "--test" | "--bench" | "--exclude"
            | "--features" => {
                parsed.cargo_args.push(value.to_string());
                index += 1;
                let argument = words.get(index).ok_or_else(|| {
                    format!("failed to parse `inspect` test input: {value} requires an argument")
                })?;
                parsed.cargo_args.push(argument.value.clone());
            }
            value
                if [
                    "--package=",
                    "--bin=",
                    "--example=",
                    "--test=",
                    "--bench=",
                    "--exclude=",
                    "--features=",
                ]
                .iter()
                .any(|prefix| value.starts_with(prefix) && value.len() > prefix.len()) =>
            {
                parsed.cargo_args.push(value.to_string());
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "failed to parse `inspect` test input: unsupported cargo test option `{value}`"
                ));
            }
            filter => parsed.filters.push(filter.to_string()),
        }
        index += 1;
    }

    if parsed.filters.is_empty()
        && parsed
            .libtest_args
            .iter()
            .any(|argument| argument == "--exact")
    {
        return Err(
            "failed to parse `inspect` test input: --exact requires at least one test filter"
                .to_string(),
        );
    }

    Ok(parsed)
}

fn execute_cargo_test_command(cwd: &str, command: &str) -> InspectCommandExecution {
    let parsed = match parse_cargo_test_command(command) {
        Ok(parsed) => parsed,
        Err(message) => return inspect_command_failed(NativeToolExecutionOutput::same(message)),
    };
    let filters = if parsed.filters.is_empty() {
        vec![None]
    } else {
        parsed.filters.iter().map(Some).collect::<Vec<_>>()
    };
    let label_filters = filters.len() > 1;
    let mut formatted = String::new();
    let mut succeeded = true;

    for filter in filters {
        let mut args = parsed.cargo_args.clone();
        if let Some(filter) = filter {
            args.push(filter.clone());
        }
        if !parsed.libtest_args.is_empty() {
            args.push("--".to_string());
            args.extend(parsed.libtest_args.iter().cloned());
        }

        let output = match Command::new("cargo").args(&args).current_dir(cwd).output() {
            Ok(output) => output,
            Err(error) => {
                return inspect_command_failed(NativeToolExecutionOutput::same(format!(
                    "{formatted}failed to execute `cargo test`: {error}"
                )));
            }
        };
        if label_filters {
            let _ = writeln!(
                formatted,
                "filter {}",
                filter.expect("multiple test invocations always have filters")
            );
        }
        formatted.push_str(&format_cargo_test_output(
            output.status,
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ));
        if !formatted.ends_with('\n') {
            formatted.push('\n');
        }

        succeeded &= output.status.success();
        if !output.status.success()
            && !rust_error_locations(&String::from_utf8_lossy(&output.stderr)).is_empty()
        {
            break;
        }
    }

    InspectCommandExecution {
        output: NativeToolExecutionOutput::same(formatted),
        succeeded,
    }
}

fn format_cargo_test_output(status: ExitStatus, stdout: &str, stderr: &str) -> String {
    let diagnostics = rust_error_locations(stderr);
    if !diagnostics.is_empty() {
        return format_rust_diagnostics(&diagnostics);
    }

    let summary = cargo_test_summary(stdout);
    if status.success() {
        return summary
            .map(|summary| format_cargo_test_summary("ok", summary))
            .unwrap_or_else(|| "ok\n".to_string());
    }

    let failures = rust_test_failure_sections(stdout);
    let runtime_failures = rust_test_runtime_failure_sections(stderr);
    if failures.is_empty() && runtime_failures.is_empty() {
        return format!("cargo test failed {}\n", output_status_text(status));
    }

    let mut output = String::from("Test failures\n");
    output.push_str(&failures);
    if !failures.is_empty() && !runtime_failures.is_empty() {
        output.push('\n');
    }
    output.push_str(&runtime_failures);
    if let Some(summary) = summary {
        output.push_str(&format_cargo_test_summary("FAILED", summary));
    } else {
        let _ = writeln!(output, "cargo test failed {}", output_status_text(status));
    }
    output
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CargoTestSummary {
    passed: usize,
    failed: usize,
    ignored: usize,
    measured: usize,
    filtered_out: usize,
}

fn cargo_test_summary(stdout: &str) -> Option<CargoTestSummary> {
    let mut total = CargoTestSummary::default();
    let mut found = false;
    for line in stdout.lines() {
        let Some(result) = line.trim().strip_prefix("test result: ") else {
            continue;
        };
        let Some((_, counts)) = result.split_once(". ") else {
            continue;
        };
        let mut parsed = CargoTestSummary::default();
        for count in counts.split(';') {
            let count = count.trim();
            if let Some(value) = test_summary_count(count, " passed") {
                parsed.passed = value;
            } else if let Some(value) = test_summary_count(count, " failed") {
                parsed.failed = value;
            } else if let Some(value) = test_summary_count(count, " ignored") {
                parsed.ignored = value;
            } else if let Some(value) = test_summary_count(count, " measured") {
                parsed.measured = value;
            } else if let Some(value) = test_summary_count(count, " filtered out") {
                parsed.filtered_out = value;
            }
        }
        total.passed += parsed.passed;
        total.failed += parsed.failed;
        total.ignored += parsed.ignored;
        total.measured += parsed.measured;
        total.filtered_out += parsed.filtered_out;
        found = true;
    }
    found.then_some(total)
}

fn test_summary_count(value: &str, suffix: &str) -> Option<usize> {
    value.strip_suffix(suffix)?.parse().ok()
}

fn format_cargo_test_summary(status: &str, summary: CargoTestSummary) -> String {
    format!(
        "{status}: {} passed; {} failed; {} ignored; {} measured; {} filtered out\n",
        summary.passed, summary.failed, summary.ignored, summary.measured, summary.filtered_out
    )
}

fn rust_test_failure_sections(stdout: &str) -> String {
    let lines = stdout.lines().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|line| line.strip_suffix(" stdout ----"))
        else {
            index += 1;
            continue;
        };

        if !output.is_empty() {
            output.push('\n');
        }
        let _ = writeln!(output, "{name}");
        index += 1;
        while index < lines.len() {
            let body_line = lines[index];
            let trimmed = body_line.trim();
            if (trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----"))
                || trimmed == "failures:"
            {
                break;
            }
            if !trimmed.starts_with("note: run with `RUST_BACKTRACE=") {
                let _ = writeln!(output, "{body_line}");
            }
            index += 1;
        }
        while output.ends_with("\n\n") {
            output.pop();
        }
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

fn rust_test_runtime_failure_sections(stderr: &str) -> String {
    let mut output = String::new();
    let mut in_failure = false;

    for line in stderr.lines() {
        let trimmed = line.trim();
        if !in_failure {
            if trimmed.starts_with("thread '")
                && (trimmed.contains("' panicked at ")
                    || trimmed.ends_with("' has overflowed its stack"))
            {
                let _ = writeln!(output, "{trimmed}");
                in_failure = true;
            }
            continue;
        }

        if trimmed.starts_with("error: test failed") || trimmed == "Caused by:" {
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with("note: run with `RUST_BACKTRACE=") {
            continue;
        }
        let _ = writeln!(output, "{line}");
    }

    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RustErrorLocation {
    code: String,
    summary: String,
    path: String,
    line: usize,
    column: usize,
}

fn rust_error_locations(stderr: &str) -> Vec<RustErrorLocation> {
    let mut locations = Vec::new();
    let mut current_error: Option<(String, String)> = None;

    for line in stderr.lines() {
        let trimmed = line.trim_start();
        if let Some(parsed) = rust_error_header(trimmed) {
            current_error = Some(parsed);
            continue;
        }

        let Some((code, summary)) = current_error.clone() else {
            continue;
        };
        let Some((path, line_number, column)) = rust_location_line(trimmed) else {
            continue;
        };
        locations.push(RustErrorLocation {
            code,
            summary,
            path,
            line: line_number,
            column,
        });
        current_error = None;
    }

    locations
}

fn format_rust_diagnostics(diagnostics: &[RustErrorLocation]) -> String {
    let mut output = String::from("E0 err lineposition\n");
    let mut paths = Vec::<&str>::new();
    for diagnostic in diagnostics {
        if !paths.contains(&diagnostic.path.as_str()) {
            paths.push(&diagnostic.path);
        }
    }

    for path in paths {
        let _ = writeln!(output, "{path}");
        for diagnostic in diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.path == path)
        {
            let _ = writeln!(
                output,
                "{} {} {}:{}",
                diagnostic.code, diagnostic.summary, diagnostic.line, diagnostic.column
            );
        }
    }
    output
}

fn rust_error_header(line: &str) -> Option<(String, String)> {
    if let Some(rest) = line.strip_prefix("error[E0") {
        let digits = rest
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() || !rest[digits.len()..].starts_with("]:") {
            return None;
        }
        let summary = rest[digits.len() + 2..].trim();
        if summary.is_empty() {
            return None;
        }
        return Some((digits, summary.to_string()));
    }

    let summary = line.strip_prefix("error:")?.trim();
    if summary.is_empty() {
        return None;
    }
    Some(("0".to_string(), summary.to_string()))
}

fn rust_location_line(line: &str) -> Option<(String, usize, usize)> {
    let location = line.strip_prefix("--> ")?;
    let mut parts = location.rsplitn(3, ':');
    let column = parts.next()?.parse::<usize>().ok()?;
    let line_number = parts.next()?.parse::<usize>().ok()?;
    let path = parts.next()?.to_string();
    Some((path, line_number, column))
}

fn format_line_anchor(line_number: usize, hash: u8) -> String {
    format!("{line_number} {}", edit_anchor_word(hash))
}

fn edit_line_hash(line: &str) -> u8 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    hash = edit_hash_bytes(hash, line.as_bytes());
    (hash & 0xff) as u8
}

fn edit_hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn build_locator_repository_map(cwd: &str) -> Result<String, String> {
    let root = PathBuf::from(cwd);
    let mut stack = vec![root.clone()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|error| {
            format!(
                "failed to read repository directory {}: {error}",
                dir.display()
            )
        })?;
        let mut entries = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries.into_iter().rev() {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if path.is_dir() {
                if !should_skip_locator_dir(name) {
                    stack.push(path);
                }
            } else if locator_file_is_supported(&path) {
                files.push(path);
                if files.len() >= LOCATOR_MAX_FILES {
                    break;
                }
            }
        }
        if files.len() >= LOCATOR_MAX_FILES {
            break;
        }
    }

    let mut map = format!("root: {}\nfiles:\n", root.display());
    for path in files {
        if map.len() >= LOCATOR_MAX_BYTES {
            map.push_str("- <repository map truncated>\n");
            break;
        }
        let relative = path.strip_prefix(&root).unwrap_or(&path);
        map.push_str(&format!("- {}\n", relative.display()));
        if let Ok(text) = fs::read_to_string(&path) {
            let symbols = locator_symbols(&text);
            for (line_number, symbol) in symbols.into_iter().take(LOCATOR_MAX_SYMBOLS_PER_FILE) {
                map.push_str(&format!("  L{line_number}: {symbol}\n"));
                if map.len() >= LOCATOR_MAX_BYTES {
                    map.push_str("  <repository map truncated>\n");
                    return Ok(map);
                }
            }
        }
    }
    Ok(map)
}

fn should_skip_locator_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | ".idea" | ".vscode" | "__pycache__"
    )
}

fn locator_file_is_supported(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|file_name| file_name.to_str()) else {
        return false;
    };
    if matches!(file_name, "Cargo.toml" | "README.md") {
        return true;
    }
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("rs" | "toml" | "md")
    )
}

fn locator_symbols(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            if locator_line_is_symbol(trimmed) {
                Some((index + 1, trimmed.chars().take(160).collect()))
            } else {
                None
            }
        })
        .collect()
}

fn locator_line_is_symbol(line: &str) -> bool {
    line.starts_with("pub struct ")
        || line.starts_with("struct ")
        || line.starts_with("pub enum ")
        || line.starts_with("enum ")
        || line.starts_with("pub trait ")
        || line.starts_with("trait ")
        || line.starts_with("pub fn ")
        || line.starts_with("fn ")
        || line.starts_with("impl ")
        || line.starts_with("pub mod ")
        || line.starts_with("mod ")
        || line.starts_with("pub const ")
        || line.starts_with("const ")
}

fn execute_edit_file_tool(cwd: &str, input: &str) -> NativeToolExecutionOutput {
    let request = match parse_edit_file_tool_input(input) {
        Ok(request) => request,
        Err(message) => return NativeToolExecutionOutput::same(message),
    };
    match apply_edit_file_tool_request_with_display(cwd, &request) {
        Ok(output) => NativeToolExecutionOutput::split(output.model_output, output.display_output),
        Err(message) => NativeToolExecutionOutput::same(message),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedEditFileToolRequest {
    model_output: String,
    display_output: String,
}

#[cfg(test)]
fn apply_edit_file_tool_request(
    cwd: &str,
    request: &EditFileToolRequest,
) -> Result<String, String> {
    Ok(apply_edit_file_tool_request_with_display(cwd, request)?.model_output)
}

fn apply_edit_file_tool_request_with_display(
    cwd: &str,
    request: &EditFileToolRequest,
) -> Result<AppliedEditFileToolRequest, String> {
    let mut errors = Vec::new();

    for (index, operation) in request.operations.iter().enumerate() {
        if let Err(message) = apply_edit_file_operation(cwd, operation) {
            errors.push(format!(
                "{} {}",
                index + 1,
                compact_edit_file_error(&message)
            ));
        }
    }

    if errors.is_empty() {
        return Ok(AppliedEditFileToolRequest {
            model_output: "ok".to_string(),
            display_output: "ok".to_string(),
        });
    }

    let mut model_output = String::from("edit errors\n");
    model_output.push_str(&errors.join("\n"));
    if !model_output.ends_with('\n') {
        model_output.push('\n');
    }
    Ok(AppliedEditFileToolRequest {
        model_output: model_output.clone(),
        display_output: model_output,
    })
}
fn compact_edit_file_error(message: &str) -> String {
    message
        .strip_prefix("failed to edit ")
        .unwrap_or(message)
        .to_string()
}

fn apply_edit_file_operation(cwd: &str, operation: &EditFileOperation) -> Result<(), String> {
    match operation {
        EditFileOperation::Add { path, body } => {
            reject_git_path(path)?;
            apply_edit_file_add(cwd, path, body)
        }
        EditFileOperation::Remove { path } => {
            reject_git_path(path)?;
            apply_edit_file_remove(cwd, path)
        }
        EditFileOperation::Move { from, to } => {
            reject_git_path(from)?;
            reject_git_path(to)?;
            apply_edit_file_move(cwd, from, to)
        }
        EditFileOperation::Edit { path, segments } => {
            reject_git_path(path)?;
            apply_edit_file_segments(cwd, path, segments)
        }
    }
}

/// Reject paths that target or traverse a `.git` directory so edit tools cannot
/// mutate repository metadata.
fn reject_git_path(path: &str) -> Result<(), String> {
    if Path::new(path)
        .components()
        .any(|component| matches!(component, std::path::Component::Normal(name) if name == ".git"))
    {
        return Err(format!(
            "failed to edit {path}: paths inside `.git` are sandboxed and cannot be modified"
        ));
    }
    Ok(())
}

fn apply_edit_file_add(cwd: &str, path: &str, body: &str) -> Result<(), String> {
    let resolved = resolve_root_relative_path(cwd, path);
    if resolved.exists() {
        return Err(format!("failed to edit {path}: file already exists"));
    }
    if let Some(parent) = resolved.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    fs::write(&resolved, body)
        .map_err(|error| format!("failed to write {}: {error}", resolved.display()))?;
    Ok(())
}

fn apply_edit_file_remove(cwd: &str, path: &str) -> Result<(), String> {
    let resolved = resolve_root_relative_path(cwd, path);
    let is_home_user = if let Ok(user) = std::env::var("USER") {
        resolved == Path::new(&format!("/home/{user}"))
    } else {
        false
    };

    if is_home_user {
        return Err(format!(
            "failed to edit {path}: removing the user home directory is rejected for safety"
        ));
    }

    let metadata = fs::symlink_metadata(&resolved)
        .map_err(|error| format!("failed to read {}: {error}", resolved.display()))?;
    if metadata.is_dir() {
        if !path.ends_with('/') {
            return Err(format!(
                "failed to edit {path}: directory removal (rmdir) requires a trailing `/`"
            ));
        }
        if !Path::new(path).is_absolute() {
            return Err(format!(
                "failed to edit {path}: directory removal (rmdir) requires an absolute path"
            ));
        }
        fs::remove_dir_all(&resolved)
            .map_err(|error| format!("failed to remove {}: {error}", resolved.display()))?;
    } else {
        fs::remove_file(&resolved)
            .map_err(|error| format!("failed to remove {}: {error}", resolved.display()))?;
    }
    Ok(())
}

fn apply_edit_file_move(cwd: &str, from: &str, to: &str) -> Result<(), String> {
    let resolved_from = resolve_root_relative_path(cwd, from);
    let resolved_to = resolve_root_relative_path(cwd, to);
    if let Some(parent) = resolved_to.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    fs::rename(&resolved_from, &resolved_to).map_err(|error| {
        format!(
            "failed to move {} to {}: {error}",
            resolved_from.display(),
            resolved_to.display()
        )
    })?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedLineEdit {
    start_index: usize,
    end_index: usize,
    body: String,
}

fn apply_edit_file_segments(cwd: &str, path: &str, segments: &[EditSegment]) -> Result<(), String> {
    if segments.is_empty() {
        return Err(format!(
            "failed to edit {path}: EDIT requires at least one segment"
        ));
    }
    let resolved = resolve_root_relative_path(cwd, path);
    let text = fs::read_to_string(&resolved)
        .map_err(|error| format!("failed to read {}: {error}", resolved.display()))?;
    let lines = text.lines().collect::<Vec<_>>();
    let mut planned = Vec::new();
    for segment in segments {
        planned.push(plan_edit_segment(path, &lines, segment)?);
    }
    planned.sort_by_key(|edit| edit.start_index);
    for pair in planned.windows(2) {
        let previous = &pair[0];
        let next = &pair[1];
        if previous.end_index > next.start_index {
            return Err(format!("failed to edit {path}: edit segments overlap"));
        }
    }

    let mut output_lines = Vec::new();
    let mut cursor = 0;
    for edit in &planned {
        output_lines.extend(
            lines[cursor..edit.start_index]
                .iter()
                .map(|line| (*line).to_string()),
        );
        output_lines.extend(edit.body.lines().map(str::to_string));
        cursor = edit.end_index;
    }
    output_lines.extend(lines[cursor..].iter().map(|line| (*line).to_string()));

    let mut output = output_lines.join("\n");
    if text.ends_with('\n') || !output.is_empty() {
        output.push('\n');
    }
    fs::write(&resolved, output)
        .map_err(|error| format!("failed to write {}: {error}", resolved.display()))?;
    Ok(())
}

fn plan_edit_segment(
    path: &str,
    lines: &[&str],
    segment: &EditSegment,
) -> Result<PlannedLineEdit, String> {
    match segment {
        EditSegment::Replace { start, end, body } => {
            validate_line_anchor(path, lines, *start)?;
            validate_line_anchor(path, lines, *end)?;
            if end.line_number < start.line_number {
                return Err(format!(
                    "failed to edit {path}: end anchor precedes start anchor"
                ));
            }
            Ok(PlannedLineEdit {
                start_index: start.line_number - 1,
                end_index: end.line_number,
                body: body.clone(),
            })
        }
        EditSegment::Delete { start, end } => {
            validate_line_anchor(path, lines, *start)?;
            validate_line_anchor(path, lines, *end)?;
            if end.line_number < start.line_number {
                return Err(format!(
                    "failed to edit {path}: end anchor precedes start anchor"
                ));
            }
            Ok(PlannedLineEdit {
                start_index: start.line_number - 1,
                end_index: end.line_number,
                body: String::new(),
            })
        }
        EditSegment::Insert {
            position,
            anchor,
            body,
        } => {
            validate_line_anchor(path, lines, *anchor)?;
            if matches!(position, InsertPosition::Append) && anchor.line_number != lines.len() {
                return Err(format!(
                    "failed to edit {path}: APPEND anchor must be the current last line"
                ));
            }
            let insert_index = match position {
                InsertPosition::Before => anchor.line_number - 1,
                InsertPosition::After | InsertPosition::Append => anchor.line_number,
            };
            Ok(PlannedLineEdit {
                start_index: insert_index,
                end_index: insert_index,
                body: body.clone(),
            })
        }
    }
}

fn validate_line_anchor(path: &str, lines: &[&str], anchor: LineAnchor) -> Result<(), String> {
    if anchor.line_number == 0 || anchor.line_number > lines.len() {
        return Err(format!(
            "{path} anchor line {} outside 1..={}",
            anchor.line_number,
            lines.len()
        ));
    }
    let current_hash = edit_line_hash(lines[anchor.line_number - 1]);
    if current_hash != anchor.hash {
        return Err(format!(
            "{path} stale anchor {}",
            format_line_anchor(anchor.line_number, anchor.hash)
        ));
    }
    Ok(())
}

fn parse_edit_file_tool_input(input: &str) -> Result<EditFileToolRequest, String> {
    let parser = EditFileParser::new(input);
    parser.parse()
}

struct EditFileParser<'a> {
    input: &'a str,
    offset: usize,
    operations: Vec<EditFileOperation>,
}

impl<'a> EditFileParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            offset: 0,
            operations: Vec::new(),
        }
    }
    fn parse(mut self) -> Result<EditFileToolRequest, String> {
        if self.input.lines().any(edit_file_patch_delimiter) {
            return Err(
                "failed to parse `edit_file` input: `***` patch delimiters are not supported"
                    .to_string(),
            );
        }

        while self.offset < self.input.len() {
            let line = self.next_line().ok_or_else(|| {
                "failed to parse `edit_file` input: expected section header".to_string()
            })?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(path) = line.strip_prefix("§ Edit ") {
                let path = parse_edit_file_path(path, "Edit")?;
                let segments = self.parse_edit_segments()?;
                self.operations
                    .push(EditFileOperation::Edit { path, segments });
            } else if let Some(path) = line.strip_prefix("§ Add ") {
                let path = parse_edit_file_path(path, "Add")?;
                let body = self.take_body_until_header()?;
                self.operations.push(EditFileOperation::Add { path, body });
            } else if let Some(path) = line.strip_prefix("§ Remove ") {
                self.operations.push(EditFileOperation::Remove {
                    path: parse_edit_file_path(path, "Remove")?,
                });
            } else if let Some(from) = line.strip_prefix("§ Move ") {
                let to_line = self.next_line().ok_or_else(|| {
                    "failed to parse `edit_file` input: Move requires `§ To <path>`".to_string()
                })?;
                let Some(to) = to_line.strip_prefix("§ To ") else {
                    return Err(
                        "failed to parse `edit_file` input: Move requires `§ To <path>`"
                            .to_string(),
                    );
                };
                self.operations.push(EditFileOperation::Move {
                    from: parse_edit_file_path(from, "Move")?,
                    to: parse_edit_file_path(to, "To")?,
                });
            } else {
                return Err(format!(
                    "failed to parse `edit_file` input: unsupported section header `{line}`"
                ));
            }
        }
        if self.operations.is_empty() {
            return Err(
                "failed to parse `edit_file` input: expected at least one section".to_string(),
            );
        }
        Ok(EditFileToolRequest {
            operations: self.operations,
        })
    }

    fn parse_edit_segments(&mut self) -> Result<Vec<EditSegment>, String> {
        let mut segments = Vec::new();
        while self.offset < self.input.len() {
            let Some(line) = self.peek_line() else {
                break;
            };
            if edit_file_top_level_header(line) {
                break;
            }
            let line = self.next_line().expect("peek_line returned a line");
            if line.trim().is_empty() {
                continue;
            }
            if let Some(args) = line.strip_prefix("§ Replace ") {
                let (start, end) = parse_edit_file_anchor_pair(args, "Replace")?;
                segments.push(EditSegment::Replace {
                    start,
                    end,
                    body: self.take_body_until_header()?,
                });
            } else if let Some(args) = line.strip_prefix("§ Delete ") {
                let (start, end) = parse_edit_file_anchor_pair(args, "Delete")?;
                segments.push(EditSegment::Delete { start, end });
            } else if let Some(anchor) = line.strip_prefix("§ Before ") {
                segments.push(EditSegment::Insert {
                    position: InsertPosition::Before,
                    anchor: parse_single_edit_file_anchor(anchor, "Before")?,
                    body: self.take_body_until_header()?,
                });
            } else if let Some(anchor) = line.strip_prefix("§ After ") {
                segments.push(EditSegment::Insert {
                    position: InsertPosition::After,
                    anchor: parse_single_edit_file_anchor(anchor, "After")?,
                    body: self.take_body_until_header()?,
                });
            } else if let Some(anchor) = line.strip_prefix("§ Append ") {
                segments.push(EditSegment::Insert {
                    position: InsertPosition::Append,
                    anchor: parse_single_edit_file_anchor(anchor, "Append")?,
                    body: self.take_body_until_header()?,
                });
            } else {
                return Err(format!(
                    "failed to parse `edit_file` input: unsupported edit header `{line}`"
                ));
            }
        }
        if segments.is_empty() {
            return Err(
                "failed to parse `edit_file` input: Edit requires at least one segment".to_string(),
            );
        }
        Ok(segments)
    }

    fn next_line(&mut self) -> Option<&'a str> {
        if self.offset >= self.input.len() {
            return None;
        }
        let rest = &self.input[self.offset..];
        if let Some(index) = rest.find('\n') {
            let line = &rest[..index];
            self.offset += index + 1;
            Some(line.strip_suffix('\r').unwrap_or(line))
        } else {
            self.offset = self.input.len();
            Some(rest.strip_suffix('\r').unwrap_or(rest))
        }
    }

    fn peek_line(&self) -> Option<&'a str> {
        if self.offset >= self.input.len() {
            return None;
        }
        let rest = &self.input[self.offset..];
        if let Some(index) = rest.find('\n') {
            Some(rest[..index].strip_suffix('\r').unwrap_or(&rest[..index]))
        } else {
            Some(rest.strip_suffix('\r').unwrap_or(rest))
        }
    }

    fn take_body_until_header(&mut self) -> Result<String, String> {
        let start = self.offset;
        while self.offset < self.input.len() {
            let Some(line) = self.peek_line() else {
                break;
            };
            if edit_file_any_header(line) {
                break;
            }
            let _ = self.next_line();
        }
        decode_edit_file_body(&self.input[start..self.offset])
    }
}
fn decode_edit_file_body(body: &str) -> Result<String, String> {
    let mut decoded = String::with_capacity(body.len());
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        if character == '\\' && characters.clone().next() == Some('§') {
            let _ = characters.next();
            decoded.push('§');
        } else if character == '§' {
            return Err(
                "failed to parse `edit_file` input: literal `§` in body must be escaped as `\\§`"
                    .to_string(),
            );
        } else {
            decoded.push(character);
        }
    }
    Ok(decoded)
}

fn parse_edit_file_path(value: &str, header: &str) -> Result<String, String> {
    let path = value.trim();
    if path.is_empty() {
        return Err(format!(
            "failed to parse `edit_file` input: {header} path must not be empty"
        ));
    }
    Ok(path.to_string())
}

fn parse_edit_file_anchor_pair(
    value: &str,
    header: &str,
) -> Result<(LineAnchor, LineAnchor), String> {
    let mut parts = value.split_whitespace();
    let start_line = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires start anchor")
    })?;
    let start_word = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires start anchor")
    })?;
    let end_line = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires end anchor")
    })?;
    let end_word = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires end anchor")
    })?;
    if parts.next().is_some() {
        return Err(format!(
            "failed to parse `edit_file` input: {header} accepts exactly two anchors"
        ));
    }
    Ok((
        parse_line_anchor(&format!("{start_line} {start_word}"))?,
        parse_line_anchor(&format!("{end_line} {end_word}"))?,
    ))
}

fn parse_single_edit_file_anchor(value: &str, header: &str) -> Result<LineAnchor, String> {
    let mut parts = value.split_whitespace();
    let line_number = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires one anchor")
    })?;
    let word = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires one anchor")
    })?;
    if parts.next().is_some() {
        return Err(format!(
            "failed to parse `edit_file` input: {header} accepts exactly one anchor"
        ));
    }
    parse_line_anchor(&format!("{line_number} {word}"))
}

fn edit_file_patch_delimiter(line: &str) -> bool {
    line.starts_with("*** ")
}

fn edit_file_top_level_header(line: &str) -> bool {
    line.starts_with("§ Edit ")
        || line.starts_with("§ Add ")
        || line.starts_with("§ Remove ")
        || line.starts_with("§ Move ")
}
fn edit_file_any_header(line: &str) -> bool {
    line.starts_with("§ ")
}

fn parse_line_anchor(value: &str) -> Result<LineAnchor, String> {
    let mut parts = value.split_whitespace();
    let Some(line_number) = parts.next() else {
        return Err(
            "failed to parse `edit_file` input: anchor requires a line number and word".to_string(),
        );
    };
    let Some(word) = parts.next() else {
        return Err(
            "failed to parse `edit_file` input: anchor requires a line number and word".to_string(),
        );
    };
    if parts.next().is_some() {
        return Err(
            "failed to parse `edit_file` input: anchor accepts exactly a line number and word"
                .to_string(),
        );
    }

    let line_number = line_number.parse::<usize>().map_err(|_| {
        "failed to parse `edit_file` input: anchor line number must be positive".to_string()
    })?;
    if line_number == 0 {
        return Err(
            "failed to parse `edit_file` input: anchor line number must be positive".to_string(),
        );
    }

    let hash = (0..=u8::MAX)
        .find(|&hash| edit_anchor_word(hash) == word)
        .ok_or_else(|| {
            "failed to parse `edit_file` input: anchor word is not in the vocabulary".to_string()
        })?;
    Ok(LineAnchor { line_number, hash })
}

const EDIT_ANCHOR_VOCABULARY: &str = include_str!("../../../o200k_anchor_candidates.txt");

fn edit_anchor_word(hash: u8) -> &'static str {
    EDIT_ANCHOR_VOCABULARY
        .lines()
        .nth(hash as usize)
        .and_then(|line| line.split_once("\": \""))
        .and_then(|(_, value)| value.strip_suffix("\","))
        .or_else(|| {
            EDIT_ANCHOR_VOCABULARY
                .lines()
                .nth(hash as usize)
                .and_then(|line| line.split_once("\": \""))
                .and_then(|(_, value)| value.strip_suffix("\"}"))
        })
        .expect("edit anchor vocabulary contains all 256 entries")
}

fn parse_staged_patch_tool_input(input: &str) -> Result<String, String> {
    let mut lines = input.lines();
    let Some(line) = lines.next() else {
        return Err("failed to parse staged patch input: `patch:` line is required".to_string());
    };
    if lines.any(|line| !line.trim().is_empty()) {
        return Err(
            "failed to parse staged patch input: only one `patch:` line is allowed".to_string(),
        );
    }
    let Some(patch_id) = line
        .strip_prefix("patch:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Err("failed to parse staged patch input: `patch:` line is required".to_string());
    };
    Ok(patch_id.to_string())
}

fn resolve_root_relative_path(cwd: &str, file: &str) -> PathBuf {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        path
    } else {
        PathBuf::from(cwd).join(path)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as u64
}

fn normalized_freeform_call_record(
    call: &FunctionToolCallRecord,
) -> Option<FreeformToolCallRecord> {
    crate::tools::parse_function_wrapped_freeform_input(&call.arguments)
        .ok()
        .map(|input| FreeformToolCallRecord {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            input,
        })
}
async fn execute_freeform_tool_call(
    cwd: String,
    handler: Option<NativeToolHandler>,
    terminal: TerminalManager,
    call: FreeformToolCallRecord,
) -> NativeToolExecutionOutput {
    let FreeformToolCallRecord { name, input, .. } = call;
    match handler {
        Some(NativeToolHandler::ApplyPatch) => {
            NativeToolExecutionOutput::same(match crate::apply_patch::apply_patch(&cwd, &input) {
                Ok(output) => output,
                Err(error) => error.to_string(),
            })
        }
        Some(NativeToolHandler::Inspect) => execute_inspect_tool(cwd, &input).await,
        Some(NativeToolHandler::EditFile) => execute_edit_file_tool(&cwd, &input),
        Some(NativeToolHandler::Terminal) => terminal
            .execute_tool_output(PathBuf::from(cwd), name, input)
            .await
            .unwrap_or_else(|error| NativeToolExecutionOutput::same(error.to_string())),
        Some(NativeToolHandler::MarkTaskComplete) => NativeToolExecutionOutput::same(""),
        Some(
            NativeToolHandler::Locate
            | NativeToolHandler::StagedPatchApply
            | NativeToolHandler::StagedPatchDiscard,
        ) => NativeToolExecutionOutput::same(format!("{name} is only available in root execution")),
        None => NativeToolExecutionOutput::same(format!(
            "freeform tool execution is not implemented: {name}"
        )),
    }
}

async fn execute_function_tool_call(
    cwd: String,
    handler: Option<NativeToolHandler>,
    terminal: TerminalManager,
    call: FunctionToolCallRecord,
) -> NativeToolExecutionOutput {
    let FunctionToolCallRecord {
        name, arguments, ..
    } = call;
    match handler {
        Some(NativeToolHandler::ApplyPatch) => match function_wrapped_freeform_input(&arguments) {
            Ok(input) => NativeToolExecutionOutput::same(
                match crate::apply_patch::apply_patch(&cwd, &input) {
                    Ok(output) => output,
                    Err(error) => error.to_string(),
                },
            ),
            Err(output) => output,
        },
        Some(NativeToolHandler::Inspect) => match function_wrapped_freeform_input(&arguments) {
            Ok(input) => execute_inspect_tool(cwd, &input).await,
            Err(output) => output,
        },
        Some(NativeToolHandler::EditFile) => match function_wrapped_freeform_input(&arguments) {
            Ok(input) => execute_edit_file_tool(&cwd, &input),
            Err(output) => output,
        },
        Some(NativeToolHandler::Terminal) => match function_wrapped_freeform_input(&arguments) {
            Ok(input) => terminal
                .execute_tool_output(PathBuf::from(cwd), name, input)
                .await
                .unwrap_or_else(|error| NativeToolExecutionOutput::same(error.to_string())),
            Err(output) => output,
        },
        Some(NativeToolHandler::MarkTaskComplete) => NativeToolExecutionOutput::same(""),
        Some(NativeToolHandler::Locate)
        | Some(NativeToolHandler::StagedPatchApply)
        | Some(NativeToolHandler::StagedPatchDiscard) => {
            NativeToolExecutionOutput::same(format!("{name} is only available in root execution"))
        }
        None => NativeToolExecutionOutput::same(format!(
            "function tool execution is not implemented: {name}"
        )),
    }
}

fn function_wrapped_freeform_input(arguments: &str) -> Result<String, NativeToolExecutionOutput> {
    crate::tools::parse_function_wrapped_freeform_input(arguments).map_err(|error| {
        NativeToolExecutionOutput::same(format!("invalid function tool arguments: {error}"))
    })
}

#[cfg(test)]
mod tests;
