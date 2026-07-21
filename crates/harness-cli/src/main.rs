//! Command-line entrypoint for interactive harness sessions.

mod commands;

use std::{
    env,
    ffi::OsString,
    fmt::Write as _,
    fs,
    future::Future,
    io,
    io::Write as IoWrite,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::Stream;
use harness_conversation_runtime::{
    ConversationRuntime, ModelRequestBuilder, RuntimeConfiguration, RuntimeEffect, RuntimeError,
    RuntimePorts,
};
use harness_model_api::{
    ModelAttempt, ModelAttemptHandle, ModelFailure, ModelInput, ModelMessageRole, ModelRequest,
    ModelRequestId, ModelSelection, ModelTransport, ProviderGeneration, ResolvedModelRoute,
};
use harness_chat_completions_transport::{
    ChatStreamChunk, ChatStreamError, ChatStreamingClient, ChatTransportConfiguration,
    ChatCompletionsTransport,
};
use harness_provider::{
    ProviderAuthConfig, ProviderConfig, ProviderDriverConfig, ProviderError, ProviderIdentity,
    ProviderProfileId, ProviderRoutes, ProviderSelection, ProviderSelectionStore, ResolvedProvider,
};
use harness_responses_api::{
    ApiProvider, Auth, AuthError, ChatGptAuthTokens, CodexHeaders, ManagedChatGptAuth,
    ResponsesApiError, ResponsesStreamEvent, ResponsesStreamRequest, lean_codex_default_headers,
};
use harness_responses_protocol::{encode_input, encode_tools};
use harness_responses_transport::{
    StreamChunk, StreamError, StreamingClient, SupervisedModelTransport, TransportConfiguration,
    ws::{ResponsesWsPool, WsPoolConfig},
};
use harness_runtime_api::{
    RuntimeCommandSender, RuntimeEventReceiver, TranscriptSnapshotEntry, channel_pair,
};
use harness_session_store::{
    SessionPayload, SessionReader, SessionRecord, SessionStore, SessionStoreError, SessionWriter,
};
use harness_tool_api::{
    AvailabilityToolExecutor, ToolAvailability, ToolExecutor, ToolFailure, ToolRegistry,
};
use harness_tool_execution::{ToolInventory, WorkspaceRoot};
use harness_tui_rewrite::domain::{
    ExternalText, InitialState, ModelState, ProviderKind, ProviderState, ProviderTransport,
};
use http_body_util::BodyExt;
use sonic_rs::JsonValueTrait;

use crate::commands::{
    CommandContext, CommandError, CommandRegistry, CommandResult, CommandSpec, Dispatch,
};

// Serializable representation of SessionPayload
#[derive(serde::Serialize, serde::Deserialize)]
struct SerializableRecord {
    sequence: u64,
    payload: SerializablePayload,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
enum SerializablePayload {
    Metadata {
        title: String,
    },
    ProviderBinding {
        provider: String,
        model: String,
    },
    TurnStarted {
        turn_id: u64,
    },
    InputMessage {
        turn_id: u64,
        text: String,
    },
    ModelAttemptStarted {
        turn_id: u64,
        attempt_id: u64,
    },
    AssistantMessage {
        turn_id: u64,
        text: String,
    },
    Reasoning {
        turn_id: u64,
        content: Option<String>,
        encrypted_content: Option<String>,
        summary: Option<String>,
    },
    Error {
        turn_id: u64,
        category: SerializableErrorCategory,
        message: String,
    },
    ToolCallAccepted {
        turn_id: u64,
        call_id: String,
        name: String,
        input: String,
    },
    ToolExecutionStarted {
        turn_id: u64,
        call_id: String,
    },
    ToolExecutionFinished {
        turn_id: u64,
        call_id: String,
        output: String,
    },
    TurnFinished {
        turn_id: u64,
        outcome: SerializableTurnOutcome,
    },
    CompactionCheckpoint {
        source_revision: u64,
        summary: String,
    },
    SessionClosed,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
enum SerializableErrorCategory {
    Model,
    Protocol,
    Tool,
    Lifecycle,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
enum SerializableTurnOutcome {
    Completed,
    Interrupted { reason: String },
    Cancelled { reason: String },
    Failed { message: String },
}

fn to_serializable_payload(payload: &SessionPayload) -> SerializablePayload {
    match payload {
        SessionPayload::Metadata(m) => SerializablePayload::Metadata {
            title: m.title.clone(),
        },
        SessionPayload::ProviderBinding(p) => SerializablePayload::ProviderBinding {
            provider: p.provider.clone(),
            model: p.model.clone(),
        },
        SessionPayload::TurnStarted { turn_id } => {
            SerializablePayload::TurnStarted { turn_id: *turn_id }
        }
        SessionPayload::InputMessage { turn_id, text } => SerializablePayload::InputMessage {
            turn_id: *turn_id,
            text: text.clone(),
        },
        SessionPayload::ModelAttemptStarted {
            turn_id,
            attempt_id,
        } => SerializablePayload::ModelAttemptStarted {
            turn_id: *turn_id,
            attempt_id: *attempt_id,
        },
        SessionPayload::AssistantMessage { turn_id, text } => {
            SerializablePayload::AssistantMessage {
                turn_id: *turn_id,
                text: text.clone(),
            }
        }
        SessionPayload::Reasoning {
            turn_id,
            content,
            encrypted_content,
            summary,
        } => SerializablePayload::Reasoning {
            turn_id: *turn_id,
            content: content.clone(),
            encrypted_content: encrypted_content.clone(),
            summary: summary.clone(),
        },
        SessionPayload::Error {
            turn_id,
            category,
            message,
        } => SerializablePayload::Error {
            turn_id: *turn_id,
            category: match category {
                harness_session_store::SessionErrorCategory::Model => {
                    SerializableErrorCategory::Model
                }
                harness_session_store::SessionErrorCategory::Protocol => {
                    SerializableErrorCategory::Protocol
                }
                harness_session_store::SessionErrorCategory::Tool => {
                    SerializableErrorCategory::Tool
                }
                harness_session_store::SessionErrorCategory::Lifecycle => {
                    SerializableErrorCategory::Lifecycle
                }
            },
            message: message.clone(),
        },
        SessionPayload::ToolCallAccepted {
            turn_id,
            call_id,
            name,
            input,
        } => SerializablePayload::ToolCallAccepted {
            turn_id: *turn_id,
            call_id: call_id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        SessionPayload::ToolExecutionStarted { turn_id, call_id } => {
            SerializablePayload::ToolExecutionStarted {
                turn_id: *turn_id,
                call_id: call_id.clone(),
            }
        }
        SessionPayload::ToolExecutionFinished {
            turn_id,
            call_id,
            output,
        } => SerializablePayload::ToolExecutionFinished {
            turn_id: *turn_id,
            call_id: call_id.clone(),
            output: output.clone(),
        },
        SessionPayload::TurnFinished { turn_id, outcome } => SerializablePayload::TurnFinished {
            turn_id: *turn_id,
            outcome: match outcome {
                harness_session_store::TurnOutcome::Completed => SerializableTurnOutcome::Completed,
                harness_session_store::TurnOutcome::Interrupted { reason } => {
                    SerializableTurnOutcome::Interrupted {
                        reason: reason.clone(),
                    }
                }
                harness_session_store::TurnOutcome::Cancelled { reason } => {
                    SerializableTurnOutcome::Cancelled {
                        reason: reason.clone(),
                    }
                }
                harness_session_store::TurnOutcome::Failed { message } => {
                    SerializableTurnOutcome::Failed {
                        message: message.clone(),
                    }
                }
            },
        },
        SessionPayload::CompactionCheckpoint {
            source_revision,
            summary,
        } => SerializablePayload::CompactionCheckpoint {
            source_revision: *source_revision,
            summary: summary.clone(),
        },
        SessionPayload::SessionClosed => SerializablePayload::SessionClosed,
    }
}

fn from_serializable_payload(sp: SerializablePayload) -> SessionPayload {
    match sp {
        SerializablePayload::Metadata { title } => {
            SessionPayload::Metadata(harness_session_store::SessionMetadata { title })
        }
        SerializablePayload::ProviderBinding { provider, model } => {
            SessionPayload::ProviderBinding(harness_session_store::SessionProviderBinding {
                provider,
                model,
            })
        }
        SerializablePayload::TurnStarted { turn_id } => SessionPayload::TurnStarted { turn_id },
        SerializablePayload::InputMessage { turn_id, text } => {
            SessionPayload::InputMessage { turn_id, text }
        }
        SerializablePayload::ModelAttemptStarted {
            turn_id,
            attempt_id,
        } => SessionPayload::ModelAttemptStarted {
            turn_id,
            attempt_id,
        },
        SerializablePayload::AssistantMessage { turn_id, text } => {
            SessionPayload::AssistantMessage { turn_id, text }
        }
        SerializablePayload::Reasoning {
            turn_id,
            content,
            encrypted_content,
            summary,
        } => SessionPayload::Reasoning {
            turn_id,
            content,
            encrypted_content,
            summary,
        },
        SerializablePayload::Error {
            turn_id,
            category,
            message,
        } => SessionPayload::Error {
            turn_id,
            category: match category {
                SerializableErrorCategory::Model => {
                    harness_session_store::SessionErrorCategory::Model
                }
                SerializableErrorCategory::Protocol => {
                    harness_session_store::SessionErrorCategory::Protocol
                }
                SerializableErrorCategory::Tool => {
                    harness_session_store::SessionErrorCategory::Tool
                }
                SerializableErrorCategory::Lifecycle => {
                    harness_session_store::SessionErrorCategory::Lifecycle
                }
            },
            message,
        },
        SerializablePayload::ToolCallAccepted {
            turn_id,
            call_id,
            name,
            input,
        } => SessionPayload::ToolCallAccepted {
            turn_id,
            call_id,
            name,
            input,
        },
        SerializablePayload::ToolExecutionStarted { turn_id, call_id } => {
            SessionPayload::ToolExecutionStarted { turn_id, call_id }
        }
        SerializablePayload::ToolExecutionFinished {
            turn_id,
            call_id,
            output,
        } => SessionPayload::ToolExecutionFinished {
            turn_id,
            call_id,
            output,
        },
        SerializablePayload::TurnFinished { turn_id, outcome } => SessionPayload::TurnFinished {
            turn_id,
            outcome: match outcome {
                SerializableTurnOutcome::Completed => harness_session_store::TurnOutcome::Completed,
                SerializableTurnOutcome::Interrupted { reason } => {
                    harness_session_store::TurnOutcome::Interrupted { reason }
                }
                SerializableTurnOutcome::Cancelled { reason } => {
                    harness_session_store::TurnOutcome::Cancelled { reason }
                }
                SerializableTurnOutcome::Failed { message } => {
                    harness_session_store::TurnOutcome::Failed { message }
                }
            },
        },
        SerializablePayload::CompactionCheckpoint {
            source_revision,
            summary,
        } => SessionPayload::CompactionCheckpoint {
            source_revision,
            summary,
        },
        SerializablePayload::SessionClosed => SessionPayload::SessionClosed,
    }
}

fn to_serializable_record(record: &SessionRecord) -> SerializableRecord {
    SerializableRecord {
        sequence: record.sequence,
        payload: to_serializable_payload(&record.payload),
    }
}

fn from_serializable_record(
    sr: SerializableRecord,
    session_id: &harness_session_store::SessionId,
) -> SessionRecord {
    SessionRecord {
        session_id: session_id.clone(),
        sequence: sr.sequence,
        payload: from_serializable_payload(sr.payload),
    }
}

struct FileSessionStore {
    root: PathBuf,
}

impl SessionStore for FileSessionStore {
    fn reader(&self) -> Result<Box<dyn SessionReader>, SessionStoreError> {
        Ok(Box::new(FileSessionReader {
            root: self.root.clone(),
        }))
    }

    fn writer(
        &self,
        session_id: harness_session_store::SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn SessionWriter>, SessionStoreError>> + Send + '_>>
    {
        let root = self.root.clone();
        Box::pin(async move {
            let session_dir = root.join("sessions");
            tokio::fs::create_dir_all(&session_dir)
                .await
                .map_err(SessionStoreError::Io)?;
            let path = session_dir.join(format!("{}.json", session_id.as_str()));

            let mut records = Vec::new();
            let mut next_sequence = 1;
            if path.exists() {
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(SessionStoreError::Io)?;
                let loaded = serde_json::from_str::<Vec<SerializableRecord>>(&content)
                    .map_err(|error| SessionStoreError::InvalidFormat(error.to_string()))?;
                for sr in loaded {
                    records.push(from_serializable_record(sr, &session_id));
                }
                if let Some(last) = records.last() {
                    next_sequence = last.sequence.checked_add(1).ok_or_else(|| {
                        SessionStoreError::InvalidFormat(
                            "session sequence space is exhausted".to_string(),
                        )
                    })?;
                }
            }

            Ok(Box::new(FileSessionWriter {
                session_id,
                path,
                records,
                next_sequence,
            }) as Box<dyn SessionWriter>)
        })
    }

    fn session_path(
        &self,
        session_id: harness_session_store::SessionId,
    ) -> Result<PathBuf, SessionStoreError> {
        Ok(self
            .root
            .join("sessions")
            .join(format!("{}.json", session_id.as_str())))
    }
}

struct FileSessionReader {
    root: PathBuf,
}

impl SessionReader for FileSessionReader {
    fn load_older(
        &self,
        session_id: harness_session_store::SessionId,
        before: Option<harness_session_store::SessionSequence>,
        maximum_entries: harness_session_store::PageSize,
    ) -> Result<harness_session_store::TranscriptPage, SessionStoreError> {
        let path = self
            .root
            .join("sessions")
            .join(format!("{}.json", session_id.as_str()));
        if !path.exists() {
            return Err(SessionStoreError::NotFound(session_id));
        }

        let content = std::fs::read_to_string(&path).map_err(SessionStoreError::Io)?;
        let loaded = serde_json::from_str::<Vec<SerializableRecord>>(&content)
            .map_err(|e| SessionStoreError::InvalidFormat(e.to_string()))?;

        let mut records = Vec::new();
        for sr in loaded {
            records.push(from_serializable_record(sr, &session_id));
        }

        if let Some(before_seq) = before {
            records.retain(|r| r.sequence < before_seq);
        }

        records.reverse();

        let mut entries = Vec::new();
        let mut reached_start = true;
        let mut next_before = None;

        let limit = maximum_entries.get() as usize;
        if records.len() > limit {
            reached_start = false;
            let page_records = &records[..limit];
            let Some(last_record) = page_records.last() else {
                return Err(SessionStoreError::InvalidFormat(
                    "transcript page size must be greater than zero".to_string(),
                ));
            };
            entries.extend_from_slice(page_records);
            next_before = Some(last_record.sequence);
        } else {
            entries.extend(records);
        }

        Ok(harness_session_store::TranscriptPage {
            entries,
            next_before,
            reached_start,
        })
    }
}

struct FileSessionWriter {
    session_id: harness_session_store::SessionId,
    path: PathBuf,
    records: Vec<SessionRecord>,
    next_sequence: u64,
}

impl SessionWriter for FileSessionWriter {
    fn append<'a>(
        &'a mut self,
        records: &'a [SessionPayload],
        _durability: harness_session_store::Durability,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<harness_session_store::AppendReceipt, SessionStoreError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let start_seq = self.next_sequence;
            for payload in records {
                self.records.push(SessionRecord {
                    session_id: self.session_id.clone(),
                    sequence: self.next_sequence,
                    payload: payload.clone(),
                });
                self.next_sequence += 1;
            }
            let end_seq = self.next_sequence - 1;

            let mut serializable = Vec::new();
            for r in &self.records {
                serializable.push(to_serializable_record(r));
            }
            let content = serde_json::to_string(&serializable)
                .map_err(|e| SessionStoreError::InvalidFormat(e.to_string()))?;
            tokio::fs::write(&self.path, content)
                .await
                .map_err(SessionStoreError::Io)?;

            Ok(harness_session_store::AppendReceipt {
                sequences: start_seq..=end_seq,
                durability: harness_session_store::Durability::Durable,
            })
        })
    }

    fn close(
        self: Box<Self>,
    ) -> Pin<Box<dyn Future<Output = Result<(), SessionStoreError>> + Send>> {
        Box::pin(async { Ok(()) })
    }
}

struct RealModelRequestBuilder {
    tool_registry: Arc<RwLock<ToolRegistry>>,
    tool_availability: Arc<RwLock<ToolAvailability>>,
    base_instructions: String,
}

impl ModelRequestBuilder for RealModelRequestBuilder {
    fn build(
        &self,
        revision: u64,
        selection: &ModelSelection,
        provider_generation: ProviderGeneration,
        history: &[SessionPayload],
        steering: &[String],
    ) -> Result<Arc<ModelRequest>, RuntimeError> {
        let registry = self
            .tool_registry
            .read()
            .map_err(|_| RuntimeError::ToolRegistryUnavailable)?;
        let is_freeform_tool = |name: &str| {
            registry.iter().any(|(_, tool)| {
                tool.definition.name.as_str() == name
                    && matches!(
                        &tool.definition.input_schema,
                        harness_tool_api::ToolInputSchema::FreeformGrammar { .. }
                    )
            })
        };

        let mut input = Vec::new();
        if !self.base_instructions.is_empty() {
            input.push(ModelInput::Message {
                role: ModelMessageRole::System,
                text: self.base_instructions.clone(),
            });
        }
        for payload in history {
            match payload {
                SessionPayload::InputMessage { text, .. } => {
                    input.push(ModelInput::Message {
                        role: ModelMessageRole::User,
                        text: text.clone(),
                    });
                }
                SessionPayload::AssistantMessage { text, .. } => {
                    input.push(ModelInput::Message {
                        role: ModelMessageRole::Assistant,
                        text: text.clone(),
                    });
                }
                SessionPayload::Reasoning {
                    content,
                    encrypted_content,
                    summary,
                    ..
                } => {
                    input.push(ModelInput::Reasoning {
                        content: content.clone(),
                        encrypted_content: encrypted_content.clone(),
                        summary: summary.clone(),
                    });
                }
                SessionPayload::Error { .. } => {}
                SessionPayload::CompactionCheckpoint { summary, .. } => {
                    input.push(ModelInput::Message {
                        role: ModelMessageRole::User,
                        text: format!("Conversation summary after compaction:\n\n{summary}"),
                    });
                }
                SessionPayload::ToolCallAccepted {
                    call_id,
                    name,
                    input: tool_input,
                    ..
                } => {
                    if is_freeform_tool(name) {
                        input.push(ModelInput::FreeformToolCall {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            input: tool_input.clone(),
                        });
                    } else {
                        input.push(ModelInput::AssistantToolCall {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            arguments: tool_input.clone(),
                        });
                    }
                }
                SessionPayload::ToolExecutionFinished {
                    call_id, output, ..
                } => {
                    if history.iter().any(|payload| {
                        matches!(
                            payload,
                            SessionPayload::ToolCallAccepted {
                                call_id: prior_call_id,
                                name: prior_name,
                                ..
                            } if prior_call_id == call_id && is_freeform_tool(prior_name)
                        )
                    }) {
                        input.push(ModelInput::FreeformToolResult {
                            call_id: call_id.clone(),
                            output: output.clone(),
                        });
                    } else {
                        input.push(ModelInput::ToolResult {
                            call_id: call_id.clone(),
                            output: output.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        for s in steering {
            input.push(ModelInput::Message {
                role: ModelMessageRole::User,
                text: s.clone(),
            });
        }

        let availability = self
            .tool_availability
            .read()
            .map_err(|_| RuntimeError::ToolAvailabilityUnavailable)?;
        let mut tools = Vec::new();
        for (_, tool) in registry.iter() {
            if availability.is_enabled(tool.definition.name.as_str()) {
                tools.push(tool.definition.clone());
            }
        }

        let gen_val = provider_generation;
        Ok(Arc::new(ModelRequest {
            request_id: ModelRequestId(0),
            provider_generation: gen_val,
            history_revision: revision,
            selection: selection.clone(),
            input: Arc::from(input),
            tools: Arc::from(tools),
        }))
    }

    fn build_compatibility(
        &self,
        revision: u64,
        selection: &ModelSelection,
        provider_generation: ProviderGeneration,
        history: &[SessionPayload],
        steering: &[String],
    ) -> Result<Arc<ModelRequest>, RuntimeError> {
        let request = self.build(
            revision,
            selection,
            provider_generation,
            history,
            steering,
        )?;
        let input = request
            .input
            .iter()
            .map(|item| match item {
                ModelInput::FreeformToolCall {
                    call_id,
                    name,
                    input,
                } => ModelInput::AssistantToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: sonic_rs::json!({ "input": input }).to_string(),
                },
                ModelInput::FreeformToolResult { call_id, output } => ModelInput::ToolResult {
                    call_id: call_id.clone(),
                    output: output.clone(),
                },
                other => other.clone(),
            })
            .collect::<Vec<_>>();
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                let mut tool = tool.clone();
                if let harness_tool_api::ToolInputSchema::FreeformGrammar {
                    syntax,
                    definition,
                } = &tool.input_schema
                {
                    let schema = sonic_rs::json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": format!(
                                    "Exact freeform tool input. Grammar syntax: {syntax:?}. Native tool instructions:\n{definition}"
                                )
                            }
                        },
                        "required": ["input"],
                        "additionalProperties": false
                    });
                    tool.input_schema = harness_tool_api::ToolInputSchema::JsonSchema(
                        harness_tool_api::JsonSchema::new(
                            sonic_rs::to_string(&schema).expect("tool schema is valid JSON"),
                        ),
                    );
                }
                tool
            })
            .collect::<Vec<_>>();
        Ok(Arc::new(ModelRequest {
            input: Arc::from(input),
            tools: Arc::from(tools),
            ..(*request).clone()
        }))
    }

    fn build_compaction(
        &self,
        revision: u64,
        selection: &ModelSelection,
        provider_generation: ProviderGeneration,
        history: &[SessionPayload],
        instruction: &str,
    ) -> Result<Arc<ModelRequest>, RuntimeError> {
        let mut request = self.build(
            revision,
            selection,
            provider_generation,
            history,
            &[instruction.to_owned()],
        )?;
        Arc::make_mut(&mut request).tools = Arc::from([]);
        Ok(request)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SerializableProviderSelection {
    provider: String,
    generation: u64,
    model: SerializableModelSelection,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SerializableModelSelection {
    provider: String,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

struct FileProviderSelectionStore {
    root: PathBuf,
}

impl ProviderSelectionStore for FileProviderSelectionStore {
    fn persist(
        &self,
        selection: ProviderSelection,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProviderError>> + Send + '_>> {
        let path = self.root.join("provider-bound.json");
        let ser = SerializableProviderSelection {
            provider: selection.provider.as_str().to_string(),
            generation: selection.generation.0,
            model: SerializableModelSelection {
                provider: selection.model.provider.as_str().to_string(),
                model: selection.model.model,
                reasoning_effort: selection.model.reasoning_effort,
                service_tier: selection.model.service_tier,
            },
        };
        Box::pin(async move {
            let serialized = serde_json::to_string(&ser)
                .map_err(|e| ProviderError::Persistence(e.to_string()))?;
            tokio::fs::write(path, serialized)
                .await
                .map_err(|e| ProviderError::Persistence(e.to_string()))?;
            Ok(())
        })
    }

    fn load(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ProviderSelection>, ProviderError>> + Send + '_>> {
        let path = self.root.join("provider-bound.json");
        Box::pin(async move {
            let data = match tokio::fs::read_to_string(&path).await {
                Ok(data) => data,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(err) => {
                    return Err(ProviderError::Persistence(format!(
                        "failed to read provider-bound.json: {err}"
                    )));
                }
            };
            let ser: SerializableProviderSelection = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Persistence(format!("invalid provider-bound.json: {e}")))?;
            let provider = harness_model_api::ProviderId::new(ser.provider)
                .map_err(|e| ProviderError::Persistence(format!("invalid provider ID in persisted selection: {e}")))?;
            let model = ModelSelection::new(
                provider.clone(),
                ser.model.model,
                ser.model.reasoning_effort,
                ser.model.service_tier,
            );
            Ok(Some(ProviderSelection {
                provider,
                generation: ProviderGeneration(ser.generation),
                model,
            }))
        })
    }
}

struct SharedTransport(Arc<dyn ModelTransport>);
impl ModelTransport for SharedTransport {
    fn start(
        &self,
        attempt: Arc<ModelAttempt>,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn ModelAttemptHandle>, ModelFailure>> + Send + '_>>
    {
        self.0.start(attempt)
    }
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), ModelFailure>> + Send + '_>> {
        self.0.shutdown()
    }
}

struct CodexWsClient {
    pool: Arc<ResponsesWsPool>,
    headers: CodexHeaders,
}

impl StreamingClient for CodexWsClient {
    fn start(
        &self,
        attempt: Arc<ModelAttempt>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Pin<Box<dyn Stream<Item = Result<StreamChunk, StreamError>> + Send>>,
                        StreamError,
                    >,
                > + Send,
        >,
    > {
        let pool = Arc::clone(&self.pool);
        let mut headers = self.headers.clone();
        headers.client_request_id = attempt.attempt_id.0.to_string();
        headers.thread_id = attempt.attempt_id.0.to_string();
        Box::pin(async move {
            let tools = encode_tools(&attempt.request.tools)
                .map_err(|error| StreamError::Transport(error.to_string()))?;
            let mut body = sonic_rs::json!({
                "type": "response.create",
                "model": attempt.request.selection.model,
                "input": encode_input(&attempt.request.input),
                "tools": tools,
                "store": false,
            });
            add_selection_options(&mut body, &attempt.request.selection, &headers.session_id);
            let request = ResponsesStreamRequest { headers, body };
            let (sender, receiver) = tokio::sync::mpsc::channel(128);
            tokio::spawn(async move {
                let result = pool
                    .stream_request(request, |event| {
                        let sender = sender.clone();
                        async move {
                            let chunk = match event {
                                ResponsesStreamEvent::Frame(value) => sonic_rs::to_vec(&value)
                                    .map(StreamChunk::Bytes)
                                    .map_err(|error| StreamError::Transport(error.to_string())),
                                ResponsesStreamEvent::Completed { .. } => Ok(StreamChunk::End),
                                ResponsesStreamEvent::ServerReasoningIncluded(_)
                                | ResponsesStreamEvent::ModelsEtag(_)
                                | ResponsesStreamEvent::ServerModel(_) => {
                                    Ok(StreamChunk::Bytes(Vec::new()))
                                }
                            };
                            let _ = sender.send(chunk).await;
                        }
                    })
                    .await;
                if let Err(error) = result {
                    let _ = sender
                        .send(Err(StreamError::Transport(error.to_string())))
                        .await;
                } else {
                    let _ = sender.send(Ok(StreamChunk::End)).await;
                }
            });

            let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|item| (item, receiver))
            });
            Ok(Box::pin(stream)
                as Pin<
                    Box<dyn Stream<Item = Result<StreamChunk, StreamError>> + Send>,
                >)
        })
    }
}

struct HttpClient {
    base_url: String,
    api_key: Option<String>,
    session_id: String,
    hyper_client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::Full<bytes::Bytes>,
    >,
}

impl HttpClient {
    fn new(base_url: String, api_key: Option<String>, session_id: impl Into<String>) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let hyper_client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(https);
        Self {
            base_url,
            api_key,
            session_id: session_id.into(),
            hyper_client,
        }
    }
}

impl ChatStreamingClient for HttpClient {
    fn start(
        &self,
        body: sonic_rs::Value,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Pin<
                            Box<dyn Stream<Item = Result<ChatStreamChunk, ChatStreamError>> + Send>,
                        >,
                        ChatStreamError,
                    >,
                > + Send
                + '_,
        >,
    > {
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let hyper_client = self.hyper_client.clone();

        Box::pin(async move {
            let url_str = format!("{}/chat/completions", base_url.trim_end_matches('/'));
            let body_bytes =
                sonic_rs::to_vec(&body).map_err(|e| ChatStreamError::Transport(e.to_string()))?;

            let mut req = http::Request::post(url_str).header("content-type", "application/json");

            if let Some(key) = &api_key {
                req = req.header("authorization", format!("Bearer {key}"));
            }

            let req = req
                .body(http_body_util::Full::new(bytes::Bytes::from(body_bytes)))
                .map_err(|e| ChatStreamError::Transport(e.to_string()))?;

            let resp = hyper_client
                .request(req)
                .await
                .map_err(|e| ChatStreamError::Transport(e.to_string()))?;

            let status = resp.status();
            if !status.is_success() {
                let err_bytes = http_body_util::BodyExt::collect(resp.into_body())
                    .await
                    .map(|c| c.to_bytes())
                    .unwrap_or_default();
                let err_msg = String::from_utf8_lossy(&err_bytes).to_string();
                if status == http::StatusCode::UNAUTHORIZED {
                    return Err(ChatStreamError::Authentication(err_msg));
                } else if status == http::StatusCode::TOO_MANY_REQUESTS {
                    return Err(ChatStreamError::RateLimited(err_msg));
                } else {
                    return Err(ChatStreamError::ProviderRejected(err_msg));
                }
            }

            let body = resp.into_body();
            let stream = futures_util::stream::unfold(Some(body), |state| async move {
                let mut body = state?;

                match body.frame().await {
                    Some(Ok(frame)) => {
                        if let Some(data) = frame.data_ref()
                            && !data.is_empty()
                        {
                            return Some((Ok(ChatStreamChunk::Bytes(data.to_vec())), Some(body)));
                        }
                        Some((Ok(ChatStreamChunk::Bytes(Vec::new())), Some(body)))
                    }
                    Some(Err(err)) => {
                        Some((Err(ChatStreamError::Transport(err.to_string())), None))
                    }
                    None => Some((Ok(ChatStreamChunk::End), None)),
                }
            });
            Ok(Box::pin(stream)
                as Pin<
                    Box<dyn Stream<Item = Result<ChatStreamChunk, ChatStreamError>> + Send>,
                >)
        })
    }
}

fn add_selection_options(body: &mut sonic_rs::Value, selection: &ModelSelection, session_id: &str) {
    body["reasoning"] = sonic_rs::json!({
        "summary": "detailed",
        "effort": selection.reasoning_effort.as_deref(),
    });
    if let Some(service_tier) = &selection.service_tier {
        body["service_tier"] = sonic_rs::json!(service_tier);
    }
    body["prompt_cache_key"] = sonic_rs::json!(session_id);
}

impl StreamingClient for HttpClient {
    fn start(
        &self,
        attempt: Arc<ModelAttempt>,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Pin<Box<dyn Stream<Item = Result<StreamChunk, StreamError>> + Send>>,
                        StreamError,
                    >,
                > + Send,
        >,
    > {
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let session_id = self.session_id.clone();
        let hyper_client = self.hyper_client.clone();

        Box::pin(async move {
            let url_str = format!("{}/responses", base_url.trim_end_matches('/'));

            let input = encode_input(&attempt.request.input);
            let tools = encode_tools(&attempt.request.tools)
                .map_err(|error| StreamError::Transport(error.to_string()))?;
            let mut body = sonic_rs::json!({
                "model": attempt.request.selection.model,
                "input": input,
                "tools": tools,
                "stream": true,
                "store": false,
            });
            add_selection_options(&mut body, &attempt.request.selection, &session_id);
            let body_bytes =
                sonic_rs::to_vec(&body).map_err(|e| StreamError::Transport(e.to_string()))?;

            let mut req = http::Request::post(url_str).header("content-type", "application/json");

            if let Some(key) = &api_key {
                req = req.header("authorization", format!("Bearer {key}"));
            }

            let req = req
                .body(http_body_util::Full::new(bytes::Bytes::from(body_bytes)))
                .map_err(|e| StreamError::Transport(e.to_string()))?;

            let resp = hyper_client
                .request(req)
                .await
                .map_err(|e| StreamError::Transport(e.to_string()))?;

            let status = resp.status();
            if !status.is_success() {
                let err_bytes = http_body_util::BodyExt::collect(resp.into_body())
                    .await
                    .map(|c| c.to_bytes())
                    .unwrap_or_default();
                let err_msg = String::from_utf8_lossy(&err_bytes);
                return Err(StreamError::Transport(format!(
                    "HTTP status {status}: {err_msg}"
                )));
            }

            let content_type = resp
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if !content_type
                .split(';')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
            {
                return Err(StreamError::Transport(format!(
                    "OpenResponses streaming response has invalid content type: {content_type}"
                )));
            }

            let body = resp.into_body();
            let stream = futures_util::stream::unfold(Some(body), |state| async move {
                let mut body = state?;

                match body.frame().await {
                    Some(Ok(frame)) => {
                        if let Some(data) = frame.data_ref()
                            && !data.is_empty()
                        {
                            return Some((Ok(StreamChunk::Bytes(data.to_vec())), Some(body)));
                        }
                        Some((Ok(StreamChunk::Bytes(Vec::new())), Some(body)))
                    }
                    Some(Err(err)) => Some((Err(StreamError::Transport(err.to_string())), None)),
                    None => Some((Ok(StreamChunk::End), None)),
                }
            });
            Ok(Box::pin(stream)
                as Pin<
                    Box<dyn Stream<Item = Result<StreamChunk, StreamError>> + Send>,
                >)
        })
    }
}

static NEXT_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
const CLI_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);

type CliResult<T> = Result<T, CliError>;

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("failed to build async runtime")]
    RuntimeBuild {
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Responses(#[from] ResponsesApiError),
    #[error(transparent)]
    Session(#[from] harness_session_store::SessionStoreError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    ProviderRuntime(#[from] harness_provider::ProviderError),
    #[error("CLI I/O failed")]
    Io {
        #[source]
        source: io::Error,
    },
    #[error("channel send error: {0:?}")]
    Channel(harness_runtime_api::RuntimeSendError),
    #[error("failed to encode experiment JSON")]
    ExperimentJson {
        #[source]
        source: sonic_rs::Error,
    },
    #[error("model `{model}` was not returned by /models")]
    MissingModel { model: String },
    #[error("HOME is required when {fallback_variable} is unset")]
    HomeRequired {
        fallback_variable: &'static str,
        #[source]
        source: env::VarError,
    },
    #[error("session `{id}` was not found")]
    SessionNotFound { id: String },
    #[error("no sessions available to resume")]
    NoSessionsAvailable,
    #[error("session id mismatch: index={index} file={file}")]
    SessionIdMismatch { index: String, file: String },
    #[error("no session selected")]
    NoSessionSelected,
    #[error(
        "invalid session selection `{selected}`; enter 1-{max}, a session id, or a cwd/latest-message filter"
    )]
    InvalidSessionSelection { selected: String, max: usize },
    #[error("failed to read current working directory")]
    CurrentDir {
        #[source]
        source: io::Error,
    },
    #[error("failed to register tool: {0}")]
    ToolRegistration(String),
    #[error(
        "unsupported arguments `{arguments}`; use [--norotate], resume [sessionid] [--norotate], inspect-session <sessionid>, probe-session-chunk <sessionid> <chunk-index>, repair-session <sessionid>, or ipc-uds <socket-path>"
    )]
    UnsupportedArguments { arguments: String },
    #[error("base instructions destination has no parent directory")]
    BaseInstructionsMissingParent,
    #[error("failed to create base instructions directory {path}")]
    CreateBaseInstructionsDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("missing base instructions: expected {source_path} or {destination}")]
    MissingBaseInstructions {
        source_path: PathBuf,
        destination: PathBuf,
    },
    #[error("failed to read harness base instructions {path}")]
    ReadBaseInstructions {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read resume startup binding from {path}")]
    ResumeStartupBinding {
        path: PathBuf,
        #[source]
        source: harness_session_store::SessionStoreError,
    },
    #[error("failed to read resume transcript page from {path}")]
    ResumeTranscriptPage {
        path: PathBuf,
        #[source]
        source: harness_session_store::SessionStoreError,
    },
}

impl From<io::Error> for CliError {
    fn from(source: io::Error) -> Self {
        Self::Io { source }
    }
}

impl From<sonic_rs::Error> for CliError {
    fn from(source: sonic_rs::Error) -> Self {
        Self::ExperimentJson { source }
    }
}

struct RuntimeHandle {
    commands: RuntimeCommandSender,
    events: RuntimeEventReceiver,
}

fn load_provider_config() -> Option<ProviderConfig> {
    let path = harness_state_dir().ok()?.join("providers.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn load_chatgpt_auth() -> Result<Auth, ProviderError> {
    let mut paths = Vec::new();
    if let Ok(root) = harness_state_dir() {
        paths.push(root.join("auth.json"));
    }
    if let Ok(home) = env::var("HOME") {
        paths.push(PathBuf::from(home).join(".codex").join("auth.json"));
    }

    for path in paths {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let value: sonic_rs::Value = sonic_rs::from_str(&content)
            .map_err(|error| ProviderError::Credentials(error.to_string()))?;
        let tokens = &value["tokens"];
        let source = if tokens.is_object() { tokens } else { &value };
        let Some(access_token) = source["access_token"]
            .as_str()
            .filter(|token| !token.is_empty())
        else {
            continue;
        };
        let tokens = ChatGptAuthTokens {
            access_token: access_token.to_string(),
            refresh_token: source["refresh_token"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            account_id: source["account_id"].as_str().map(str::to_string),
            fedramp: source["fedramp"].as_bool().unwrap_or(false),
        };
        return Ok(Auth::ChatGpt(Arc::new(ManagedChatGptAuth::new(tokens))));
    }

    Err(ProviderError::Credentials(
        "ChatGPT auth.json was not found or has no access token".to_string(),
    ))
}

async fn resolve_provider_and_transport(
    provider_config: &ProviderConfig,
    session_id: &str,
    profile_id: &ProviderProfileId,
    model_slug: &str,
    active_generation: u64,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
) -> Result<(ResolvedProvider, Arc<dyn ModelTransport>), ProviderError> {
    let profile = provider_config
        .profile(profile_id)
        .ok_or_else(|| ProviderError::InvalidProfile(format!("profile not found: {profile_id}")))?;

    let model_config = profile
        .model_configs
        .iter()
        .find(|mc| mc.slug == model_slug)
        .ok_or_else(|| ProviderError::InvalidModel(format!("model not found: {model_slug}")))?;

    let mut api_key = None;
    if let Some(auth_config) = &profile.auth {
        match auth_config {
            ProviderAuthConfig::ApiKey { credential_id } => {
                let cred_path = harness_state_dir()
                    .map_err(|e| ProviderError::Credentials(e.to_string()))?
                    .join("provider-credentials")
                    .join(credential_id);
                if let Ok(key) = std::fs::read_to_string(cred_path) {
                    api_key = Some(key.trim().to_string());
                } else {
                    return Err(ProviderError::Credentials(format!(
                        "failed to read credential {credential_id}"
                    )));
                }
            }
            ProviderAuthConfig::ChatGptHarness => {}
        }
    }

    let capabilities = harness_model_api::ModelCapabilities {
        tool_calls: model_config.supports_tools,
        freeform_tool_input: !matches!(
            &profile.driver,
            ProviderDriverConfig::ChatCompletion { .. }
        ),
        streaming: true,
    };

    let context_limits = harness_model_api::ContextLimits::new(
        model_config.context_window,
        (model_config.context_window * model_config.effective_context_window_percent) / 100,
    )
    .map_err(|e| ProviderError::InvalidModel(e.to_string()))?;

    let provider_id = harness_model_api::ProviderId::new(profile_id.as_str())
        .map_err(|error| ProviderError::InvalidModel(error.to_string()))?;
    let selection = ModelSelection::new(
        provider_id,
        model_slug,
        reasoning_effort.or_else(|| profile.default_reasoning_effort.clone()),
        service_tier.or_else(|| profile.default_service_tier.clone()),
    );

    let gen_val = ProviderGeneration(active_generation);

    let root_route = ResolvedModelRoute::new(
        selection.provider.clone(),
        gen_val,
        "root",
        selection.clone(),
    )
    .map_err(|error| ProviderError::InvalidModel(error.to_string()))?;
    let compaction_route = ResolvedModelRoute::new(
        selection.provider.clone(),
        gen_val,
        "compaction",
        selection.clone(),
    )
    .map_err(|error| ProviderError::InvalidModel(error.to_string()))?;
    let tool_summary_route = match profile.tool_output_summary_model.as_ref() {
        Some(model) => {
            let tool_selection = ModelSelection::new(selection.provider.clone(), model, None, None);
            Some(
                ResolvedModelRoute::new(
                    selection.provider.clone(),
                    gen_val,
                    "tool_summary",
                    tool_selection,
                )
                .map_err(|error| ProviderError::InvalidModel(error.to_string()))?,
            )
        }
        None => None,
    };

    let routes = ProviderRoutes {
        root: root_route,
        compaction: compaction_route,
        tool_summary: tool_summary_route,
        locator: None,
    };

    let transport: Arc<dyn ModelTransport> = match &profile.driver {
        ProviderDriverConfig::ChatCompletion {
            base_url,
            request_timeout_ms: _,
            stream_idle_timeout_ms,
        } => {
            let client = Arc::new(HttpClient::new(base_url.clone(), api_key, session_id));
            let config = ChatTransportConfiguration {
                event_capacity: 128,
                chunk_timeout: Duration::from_millis(*stream_idle_timeout_ms),
                max_event_bytes: 1_048_576,
            };
            Arc::new(
                ChatCompletionsTransport::new(client, config)
                    .map_err(|e| ProviderError::Transport(e.to_string()))?,
            )
        }
        ProviderDriverConfig::HttpsResponses {
            base_url,
            request_timeout_ms: _,
            stream_idle_timeout_ms,
        } => {
            let client = Arc::new(HttpClient::new(base_url.clone(), api_key, session_id));
            let config = TransportConfiguration {
                event_capacity: 128,
                chunk_timeout: Duration::from_millis(*stream_idle_timeout_ms),
                max_event_bytes: 1_048_576,
            };
            Arc::new(
                SupervisedModelTransport::new(client, config)
                    .map_err(|e| ProviderError::Transport(e.to_string()))?,
            )
        }
        ProviderDriverConfig::CodexWsResponses {
            base_url,
            stream_idle_timeout_ms,
        } => {
            let auth = match profile.auth.as_ref() {
                Some(ProviderAuthConfig::ApiKey { .. }) => {
                    Auth::ApiKey(api_key.ok_or_else(|| {
                        ProviderError::Credentials("Codex API key is empty".to_string())
                    })?)
                }
                Some(ProviderAuthConfig::ChatGptHarness) => load_chatgpt_auth()?,
                None => load_chatgpt_auth()?,
            };
            let provider = ApiProvider::new(base_url)
                .map_err(|error| ProviderError::Transport(error.to_string()))?;
            let pool = Arc::new(ResponsesWsPool::new(
                provider,
                auth,
                lean_codex_default_headers(),
                WsPoolConfig::default(),
            ));
            let client = Arc::new(CodexWsClient {
                pool,
                headers: CodexHeaders::for_thread(session_id, session_id, session_id),
            });
            let config = TransportConfiguration {
                event_capacity: 128,
                chunk_timeout: Duration::from_millis(*stream_idle_timeout_ms),
                max_event_bytes: 1_048_576,
            };
            Arc::new(
                SupervisedModelTransport::new(client, config)
                    .map_err(|error| ProviderError::Transport(error.to_string()))?,
            )
        }
    };

    let resolved = ResolvedProvider {
        identity: ProviderIdentity {
            id: selection.provider.clone(),
            display_name: profile.display_name.clone(),
            transport: match &profile.driver {
                ProviderDriverConfig::CodexWsResponses { .. } => "ws".to_string(),
                _ => "https".to_string(),
            },
        },
        generation: gen_val,
        selected_model: selection,
        capabilities,
        context_limits,
        routes,
        transport: Box::new(SharedTransport(Arc::clone(&transport))),
    };

    Ok((resolved, transport))
}

async fn emit_runtime_failure(
    event_tx: &harness_runtime_api::RuntimeEventSender,
    seq: &mut u64,
    category: harness_runtime_api::RuntimeFailureCategory,
    message: impl Into<String>,
) {
    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
        *seq,
        harness_runtime_api::RuntimeEvent::Failure(harness_runtime_api::RuntimeFailure {
            category,
            message: message.into(),
        }),
    );
    *seq += 1;
    let _ = event_tx.send(envelope).await;
}

async fn emit_text_message(
    user_text: &str,
    assistant_text: &str,
    runtime: &mut ConversationRuntime,
    event_tx: &harness_runtime_api::RuntimeEventSender,
    seq: &mut u64,
) {
    let _ = runtime
        .append_records(&[
            SessionPayload::InputMessage {
                turn_id: 0,
                text: user_text.to_string(),
            },
            SessionPayload::AssistantMessage {
                turn_id: 0,
                text: assistant_text.to_string(),
            },
        ])
        .await;

    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
        *seq,
        harness_runtime_api::RuntimeEvent::TranscriptAppended(TranscriptSnapshotEntry {
            sequence: Some(*seq),
            payload: harness_runtime_api::TranscriptPayload::Message {
                role: harness_runtime_api::MessageRole::User,
                text: user_text.to_string(),
            },
        }),
    );
    *seq += 1;
    let _ = event_tx.send(envelope).await;

    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
        *seq,
        harness_runtime_api::RuntimeEvent::TranscriptAppended(TranscriptSnapshotEntry {
            sequence: Some(*seq),
            payload: harness_runtime_api::TranscriptPayload::Message {
                role: harness_runtime_api::MessageRole::Assistant,
                text: assistant_text.to_string(),
            },
        }),
    );
    *seq += 1;
    let _ = event_tx.send(envelope).await;
}

#[derive(Clone)]
struct App {
    provider_config: ProviderConfig,
    active_profile: String,
    session_id: String,
}

enum AppAction {
    None,
    ShowMessage(String),
    ProviderUse {
        profile: String,
    },
    ModelUse {
        model: String,
        reasoning: Option<String>,
        tier: Option<String>,
    },
    Retry,
    SetToolAvailability { pattern: String, enabled: bool },
    SetGoal { instruction: String },
    Compact { instruction: String },
    RetryCompaction { instruction: Option<String> },
    CancelCompaction,
}

type Commands = CommandRegistry<App, AppAction>;
type Context<'a> = CommandContext<'a, App, AppAction>;

fn provider(app: &mut App, mut context: Context<'_>) -> CommandResult<AppAction> {
    let subcmd = context.args.required("subcommand")?;
    match subcmd {
        "use" => {
            let profile_name = context.args.required("profile")?;
            context.args.finish()?;

            let profile_id = ProviderProfileId::new(profile_name);
            if app.provider_config.profile(&profile_id).is_none() {
                return Err(CommandError::InvalidArgument {
                    name: "profile",
                    value: profile_name.into(),
                    reason: "profile not found in config".into(),
                });
            }

            Ok(AppAction::ProviderUse {
                profile: profile_name.to_string(),
            })
        }
        other => Err(CommandError::InvalidArgument {
            name: "subcommand",
            value: other.into(),
            reason: "expected 'use'".into(),
        }),
    }
}

fn retry(_app: &mut App, context: Context<'_>) -> CommandResult<AppAction> {
    context.args.finish()?;
    Ok(AppAction::Retry)
}

fn tool(_app: &mut App, mut context: Context<'_>) -> CommandResult<AppAction> {
    let pattern = context.args.required("pattern")?.to_owned();
    let state = context.args.required("state")?;
    context.args.finish()?;
    let enabled = match state {
        "enabled" | "enable" | "on" => true,
        "disable" | "disabled" | "off" => false,
        _ => {
            return Err(CommandError::InvalidArgument {
                name: "state",
                value: state.into(),
                reason: "expected enabled or disable".into(),
            });
        }
    };
    Ok(AppAction::SetToolAvailability { pattern, enabled })
}

fn compact(_app: &mut App, mut context: Context<'_>) -> CommandResult<AppAction> {
    let first = context.args.next();
    match first {
        Some("cancel") => {
            context.args.finish()?;
            Ok(AppAction::CancelCompaction)
        }
        Some("redo") => {
            let instruction = context.args.next().map(str::to_owned);
            context.args.finish()?;
            Ok(AppAction::RetryCompaction { instruction })
        }
        Some(first) => {
            let mut instruction = first.to_owned();
            for part in context.args {
                instruction.push(' ');
                instruction.push_str(part);
            }
            Ok(AppAction::Compact { instruction })
        }
        None => Ok(AppAction::Compact {
            instruction: String::new(),
        }),
    }
}

fn model(_app: &mut App, mut context: Context<'_>) -> CommandResult<AppAction> {
    let model_name = context.args.required("model")?;
    let reasoning = context.args.next().map(|s| s.to_string());
    let tier = context.args.next().map(|s| s.to_string());
    context.args.finish()?;

    Ok(AppAction::ModelUse {
        model: model_name.to_string(),
        reasoning,
        tier,
    })
}

fn goal(_app: &mut App, mut context: Context<'_>) -> CommandResult<AppAction> {
    let text = context.args.next();
    match text {
        Some(first) => {
            let mut instruction = first.to_owned();
            for part in context.args {
                instruction.push(' ');
                instruction.push_str(part);
            }
            Ok(AppAction::SetGoal { instruction })
        }
        None => Err(CommandError::message("usage: /goal <task>")),
    }
}

fn commands(_app: &mut App, context: Context<'_>) -> CommandResult<AppAction> {
    context.args.finish()?;

    let mut output = String::from("Available commands:\n");

    for command in context.registry.visible_commands() {
        let aliases = command
            .aliases()
            .map(|alias| format!("/{alias}"))
            .collect::<Vec<_>>()
            .join(", ");

        let alias_suffix = if aliases.is_empty() {
            String::new()
        } else {
            format!(" ({aliases})")
        };

        let _ = writeln!(
            output,
            "  /{} {}{} — {}",
            command.name(),
            command.usage(),
            alias_suffix,
            command.summary(),
        );
    }

    Ok(AppAction::ShowMessage(output))
}

fn build_commands() -> Result<Commands, Box<dyn std::error::Error>> {
    Ok(Commands::builder()
        .command(
            CommandSpec::new("commands", commands)
                .alias("help")
                .summary("List available commands"),
        )
        .command(
            CommandSpec::new("provider", provider)
                .usage("use <profile>")
                .summary("Switch active provider profile"),
        )
        .command(
            CommandSpec::new("model", model)
                .usage("<name> [reasoning] [tier]")
                .summary("Switch active model settings"),
        )
        .command(
            CommandSpec::new("retry", retry)
                .summary("Retry the current user/tool turn"),
        )
        .command(
            CommandSpec::new("tool", tool)
                .usage("<pattern> <enabled|disable>")
                .summary("Enable or disable tools matching a glob"),
        )
        .command(
            CommandSpec::new("compact", compact)
                .usage("[instruction|redo [instruction]|cancel]")
                .summary("Compact, redo, or cancel session compaction"),
        )
        .command(
            CommandSpec::new("goal", goal)
                .usage("<task>")
                .summary("Set a persisted goal; the agent loop keeps going until the model calls goal complete"),
        )
        .build()?)
}

async fn execute_app_action(
    action: AppAction,
    app_state: &mut App,
    provider_config: &ProviderConfig,
    runtime: &mut ConversationRuntime,
    command_rx: &mut harness_runtime_api::RuntimeCommandReceiver,
    commands: &harness_runtime_api::RuntimeCommandSender,
    event_tx: &harness_runtime_api::RuntimeEventSender,
    session_root: &Path,
    active_generation: &Arc<AtomicU64>,
    seq: &mut u64,
    text: &str,
) -> Result<(), String> {
    match action {
        AppAction::None => Ok(()),
        AppAction::ShowMessage(msg) => {
            emit_text_message(text, &msg, runtime, event_tx, seq).await;
            Ok(())
        }
        AppAction::ProviderUse {
            profile: profile_name,
        } => {
            let profile_id = ProviderProfileId::new(profile_name.as_str());

            let (
                model_slug,
                default_reasoning_effort,
                default_service_tier,
                display_name,
                transport_name,
            ) = {
                let profile = provider_config
                    .profile(&profile_id)
                    .ok_or_else(|| format!("Unknown provider profile: {profile_name}"))?;
                (
                    profile.default_model.clone(),
                    profile.default_reasoning_effort.clone(),
                    profile.default_service_tier.clone(),
                    profile.display_name.clone(),
                    match &profile.driver {
                        ProviderDriverConfig::CodexWsResponses { .. } => "ws".to_string(),
                        _ => "https".to_string(),
                    },
                )
            };

            let next_gen = active_generation.load(Ordering::Acquire) + 1;

            let (resolved, new_transport) = resolve_provider_and_transport(
                provider_config,
                &app_state.session_id,
                &profile_id,
                &model_slug,
                next_gen,
                None,
                None,
            )
            .await
            .map_err(|e| format!("Failed to switch provider: {e}"))?;

            runtime.update_ports(new_transport, resolved.routes.root.clone());
            runtime.update_compaction_route(resolved.routes.compaction.clone());

            let provider_id = harness_model_api::ProviderId::new(profile_id.as_str())
                .map_err(|e| format!("Invalid provider ID: {e}"))?;

            let selection = ModelSelection::new(
                provider_id,
                model_slug,
                default_reasoning_effort,
                default_service_tier,
            );

            runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::SetModel {
                    selection: selection.clone(),
                })
                .await
                .map_err(|e| format!("Failed to set model in runtime: {e}"))?;
            active_generation.store(next_gen, Ordering::Release);

            let store = FileProviderSelectionStore {
                root: session_root.to_path_buf(),
            };
            let selection_persisted = ProviderSelection {
                provider: resolved.identity.id.clone(),
                generation: harness_model_api::ProviderGeneration(next_gen),
                model: selection.clone(),
            };
            store
                .persist(selection_persisted)
                .await
                .map_err(|e| format!("Failed to persist provider selection: {e}"))?;

            app_state.active_profile = profile_name;

            let outcome_text = format!("Switched provider to {display_name} ({transport_name})");
            emit_text_message(text, &outcome_text, runtime, event_tx, seq).await;

            let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
                *seq,
                harness_runtime_api::RuntimeEvent::ProviderChanged(
                    harness_runtime_api::ProviderSummary {
                        provider: resolved.identity.id.as_str().to_string(),
                        display_name,
                        transport: transport_name,
                    },
                ),
            );
            *seq += 1;
            if event_tx.send(envelope).await.is_err() {
                return Err("Event channel closed".to_string());
            }

            let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
                *seq,
                harness_runtime_api::RuntimeEvent::ModelChanged(
                    harness_runtime_api::ModelSummary {
                        selection,
                        capabilities: resolved.capabilities,
                        context_limits: resolved.context_limits,
                    },
                ),
            );
            *seq += 1;
            if event_tx.send(envelope).await.is_err() {
                return Err("Event channel closed".to_string());
            }

            Ok(())
        }
        AppAction::Retry => {
            let effects = runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::Retry)
                .await
                .map_err(|error| format!("Failed to retry turn: {error}"))?;
            if !drive_runtime_effects(runtime, command_rx, commands, effects, event_tx, seq).await {
                return Err("Runtime event channel closed".to_string());
            }
            Ok(())
        }
        AppAction::SetGoal { instruction } => {
            let goal_text = instruction.clone();
            runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::SetGoal { instruction })
                .await
                .map_err(|error| format!("Failed to set goal: {error}"))?;
            // Submit the goal as a prompt to start the agent loop.
            let effects = runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::SubmitPrompt {
                    text: goal_text,
                })
                .await
                .map_err(|error| format!("Failed to submit goal prompt: {error}"))?;
            if !drive_runtime_effects(runtime, command_rx, commands, effects, event_tx, seq).await {
                return Err("Runtime event channel closed".to_string());
            }
            Ok(())
        }
        AppAction::SetToolAvailability { pattern, enabled } => {
            runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::SetToolAvailability {
                    pattern,
                    enabled,
                })
                .await
                .map_err(|error| format!("Failed to update tool availability: {error}"))?;
            Ok(())
        }
        AppAction::Compact { instruction } => {
            let effects = runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::Compact { instruction })
                .await
                .map_err(|error| format!("Failed to start compaction: {error}"))?;
            if !drive_runtime_effects(runtime, command_rx, commands, effects, event_tx, seq).await {
                return Err("Runtime event channel closed".to_string());
            }
            Ok(())
        }
        AppAction::RetryCompaction { instruction } => {
            let effects = runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::RetryCompaction {
                    instruction,
                })
                .await
                .map_err(|error| format!("Failed to redo compaction: {error}"))?;
            if !drive_runtime_effects(runtime, command_rx, commands, effects, event_tx, seq).await {
                return Err("Runtime event channel closed".to_string());
            }
            Ok(())
        }
        AppAction::CancelCompaction => {
            runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::CancelCompaction)
                .await
                .map_err(|error| format!("Failed to cancel compaction: {error}"))?;
            Ok(())
        }
        AppAction::ModelUse {
            model: model_name,
            reasoning,
            tier,
        } => {
            let profile_id = ProviderProfileId::new(app_state.active_profile.as_str());

            let (default_reasoning_effort, default_service_tier) = {
                let profile = provider_config
                    .profile(&profile_id)
                    .ok_or_else(|| "No active provider profile".to_string())?;
                (
                    profile.default_reasoning_effort.clone(),
                    profile.default_service_tier.clone(),
                )
            };

            let next_gen = active_generation.load(Ordering::Acquire) + 1;

            let (resolved, new_transport) = resolve_provider_and_transport(
                provider_config,
                &app_state.session_id,
                &profile_id,
                &model_name,
                next_gen,
                reasoning.clone(),
                tier.clone(),
            )
            .await
            .map_err(|e| format!("Failed to switch model: {e}"))?;

            runtime.update_ports(new_transport, resolved.routes.root.clone());
            runtime.update_compaction_route(resolved.routes.compaction.clone());

            let provider_id = harness_model_api::ProviderId::new(profile_id.as_str())
                .map_err(|e| format!("Invalid provider ID: {e}"))?;

            let selection = ModelSelection::new(
                provider_id,
                model_name.clone(),
                reasoning.or(default_reasoning_effort),
                tier.or(default_service_tier),
            );

            runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::SetModel {
                    selection: selection.clone(),
                })
                .await
                .map_err(|e| format!("Failed to set model in runtime: {e}"))?;
            active_generation.store(next_gen, Ordering::Release);

            let store = FileProviderSelectionStore {
                root: session_root.to_path_buf(),
            };
            let selection_persisted = ProviderSelection {
                provider: resolved.identity.id.clone(),
                generation: harness_model_api::ProviderGeneration(next_gen),
                model: selection.clone(),
            };
            store
                .persist(selection_persisted)
                .await
                .map_err(|e| format!("Failed to persist model selection: {e}"))?;

            let outcome_text = format!("Switched model to {model_name}");
            emit_text_message(text, &outcome_text, runtime, event_tx, seq).await;

            let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
                *seq,
                harness_runtime_api::RuntimeEvent::ModelChanged(
                    harness_runtime_api::ModelSummary {
                        selection,
                        capabilities: resolved.capabilities,
                        context_limits: resolved.context_limits,
                    },
                ),
            );
            *seq += 1;
            if event_tx.send(envelope).await.is_err() {
                return Err("Event channel closed".to_string());
            }

            Ok(())
        }
    }
}

async fn run_model_attempt(
    runtime: &mut ConversationRuntime,
    command_rx: &mut harness_runtime_api::RuntimeCommandReceiver,
    commands: &harness_runtime_api::RuntimeCommandSender,
    turn_id: u64,
    attempt: Arc<ModelAttempt>,
    route: ResolvedModelRoute,
    event_tx: &harness_runtime_api::RuntimeEventSender,
    seq: &mut u64,
) -> Result<Option<Vec<RuntimeEffect>>, RuntimeError> {
    let attempt_id = attempt.attempt_id.0;
    let compaction_attempt = runtime.compaction_attempt_active();
    ConversationRuntime::build_active_attempt(turn_id, Arc::clone(&attempt), route)?;

    let active_transport = runtime.active_transport();
    let mut handle = match active_transport.start(attempt).await {
        Ok(handle) => handle,
        Err(error) => {
            if compaction_attempt {
                runtime.fail_compaction_attempt();
                return Ok(Some(vec![RuntimeEffect::Emit(
                    harness_runtime_api::RuntimeEvent::Failure(
                        harness_runtime_api::RuntimeFailure {
                            category: harness_runtime_api::RuntimeFailureCategory::Model,
                            message: format!("{error:?}"),
                        },
                    ),
                )]));
            }
            let effects = runtime
                .finish_model_attempt(
                    turn_id,
                    attempt_id,
                    harness_model_api::ModelTerminalOutcome::Failed(error),
                )
                .await?;
            return Ok(Some(effects));
        }
    };

    let mut deferred = Vec::new();
    loop {
        // Race the command channel against the event stream so that an
        // interrupt (Esc/Ctrl-C) preempts the blocking next_event() call
        // immediately rather than waiting for the next model delta.
        enum Gate {
            Command(harness_runtime_api::RuntimeCommand),
            Event(harness_model_api::ModelEvent),
            Closed,
        }
        let gate = tokio::select! {
            cmd = command_rx.recv() => match cmd {
                Ok(cmd) => Gate::Command(cmd),
                Err(_) => Gate::Closed,
            },
            event = handle.next_event() => match event {
                Some(event) => Gate::Event(event),
                None => Gate::Closed,
            },
        };
        let event = match gate {
            Gate::Command(command) => {
                let is_interrupt = matches!(
                    command,
                    harness_runtime_api::RuntimeCommand::Interrupt { .. }
                        | harness_runtime_api::RuntimeCommand::StopRequestLoop
                        | harness_runtime_api::RuntimeCommand::AbortResponse
                );
                if !is_interrupt {
                    // QueueSteering: queue text for the next attempt and let the
                    // current stream finish naturally — do NOT cancel.
                    if let harness_runtime_api::RuntimeCommand::QueueSteering { .. } = &command {
                        if let Ok(effects) = runtime.dispatch_command(command).await {
                            for effect in effects {
                                if let RuntimeEffect::Emit(event) = effect {
                                    let envelope =
                                        harness_runtime_api::RuntimeEventEnvelope::new(*seq, event);
                                    *seq += 1;
                                    let _ = event_tx.send(envelope).await;
                                }
                            }
                        }
                        continue;
                    }
                    // Other non-interrupt commands: forward to the outer loop and
                    // cancel the stream so we get a clean Terminal event instead
                    // of the "model stream ended before a terminal event" error.
                    let _ = commands.try_send(command);
                    handle.cancel(harness_model_api::ModelCancellation {
                        reason: "non-interrupt command during streaming".to_owned(),
                    });
                    continue;
                }
                // StopRequestLoop just sets the stop-requested flag so the
                // turn ends at the next continuation — don't cancel the
                // transport, let the current response finish naturally.
                let needs_cancel = !matches!(
                    command,
                    harness_runtime_api::RuntimeCommand::StopRequestLoop
                );
                if let Err(error) = runtime.dispatch_command(command).await {
                    emit_runtime_failure(
                        event_tx,
                        seq,
                        harness_runtime_api::RuntimeFailureCategory::Command,
                        format!("runtime command failed: {error}"),
                    )
                    .await;
                }
                if needs_cancel {
                    handle.cancel(harness_model_api::ModelCancellation {
                        reason: "user interrupt".to_owned(),
                    });
                }
                // After handling a command, loop back for the next event.
                continue;
            }
            Gate::Event(event) => event,
            Gate::Closed => break,
        };
        let compatibility_failure = match &event {
            harness_model_api::ModelEvent::Terminal(outcome)
                if !compaction_attempt && is_custom_tool_compatibility_failure(outcome) =>
            {
                Some(outcome.clone())
            }
            _ => None,
        };
        let terminal = matches!(&event, harness_model_api::ModelEvent::Terminal(_));
        let (event_effects, stop) = if let Some(outcome) = compatibility_failure {
            let started = runtime.model_response_started();
            if let Some(retry) = runtime.retry_with_compatibility(turn_id, attempt_id)? {
                let mut effects = Vec::new();
                if started {
                    effects.push(RuntimeEffect::Emit(
                        harness_runtime_api::RuntimeEvent::ResponseFinished(outcome),
                    ));
                }
                effects.push(RuntimeEffect::Emit(harness_runtime_api::RuntimeEvent::Failure(
                    harness_runtime_api::RuntimeFailure {
                        category: harness_runtime_api::RuntimeFailureCategory::Model,
                        message: "The provider rejected native custom-tool history; compatibility mode will be used and the request will be retried.".to_string(),
                    },
                )));
                effects.push(retry);
                (effects, true)
            } else {
                (runtime
                    .finish_model_attempt(turn_id, attempt_id, outcome)
                    .await?, true)
            }
        } else {
            match runtime
            .dispatch_model_event(turn_id, attempt_id, event)
            .await
        {
            Ok(effects) => (effects, terminal),
            Err(error) => {
                let failure = harness_model_api::ModelTerminalOutcome::Failed(
                    harness_model_api::ModelFailure {
                        kind: harness_model_api::ModelFailureKind::Protocol,
                        message: format!("model event rejected: {error}"),
                    },
                );
                if compaction_attempt {
                    let started = runtime.model_response_started();
                    let failure_message = format!("{failure:?}");
                    runtime.fail_compaction_attempt();
                    let effect = if started {
                        RuntimeEffect::Emit(
                            harness_runtime_api::RuntimeEvent::ResponseFinished(failure),
                        )
                    } else {
                        RuntimeEffect::Emit(harness_runtime_api::RuntimeEvent::Failure(
                            harness_runtime_api::RuntimeFailure {
                                category: harness_runtime_api::RuntimeFailureCategory::Protocol,
                                message: failure_message,
                            },
                        ))
                    };
                    (vec![effect], true)
                } else {
                    (
                        runtime
                            .finish_model_attempt(turn_id, attempt_id, failure)
                            .await?,
                        true,
                    )
                }
            }
        }
        };

        for effect in event_effects {
            match effect {
                RuntimeEffect::CommitAssistant {
                    turn_id,
                    attempt_id,
                } => {
                    let commit_effects = runtime.commit_assistant(turn_id, attempt_id).await?;
                    for commit_effect in commit_effects {
                        match commit_effect {
                            RuntimeEffect::Emit(event) => {
                                let envelope =
                                    harness_runtime_api::RuntimeEventEnvelope::new(*seq, event);
                                *seq += 1;
                                if event_tx.send(envelope).await.is_err() {
                                    return Ok(None);
                                }
                            }
                            effect => deferred.push(effect),
                        }
                    }
                }
                RuntimeEffect::Emit(event) => {
                    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(*seq, event);
                    *seq += 1;
                    if event_tx.send(envelope).await.is_err() {
                        return Ok(None);
                    }
                }
                effect => deferred.push(effect),
            }
        }

        if stop {
            return Ok(Some(deferred));
        }
    }

    let failure =
        harness_model_api::ModelTerminalOutcome::Failed(harness_model_api::ModelFailure {
            kind: harness_model_api::ModelFailureKind::Protocol,
            message: "model stream ended before a terminal event".to_string(),
        });
    if compaction_attempt {
        let started = runtime.model_response_started();
        let failure_message = format!("{failure:?}");
        runtime.fail_compaction_attempt();
        let event = if started {
            harness_runtime_api::RuntimeEvent::ResponseFinished(failure)
        } else {
            harness_runtime_api::RuntimeEvent::Failure(harness_runtime_api::RuntimeFailure {
                category: harness_runtime_api::RuntimeFailureCategory::Protocol,
                message: failure_message,
            })
        };
        return Ok(Some(vec![RuntimeEffect::Emit(event)]));
    }
    let effects = runtime
        .finish_model_attempt(turn_id, attempt_id, failure)
        .await?;
    deferred.extend(effects);
    Ok(Some(deferred))
}

fn is_custom_tool_compatibility_failure(
    outcome: &harness_model_api::ModelTerminalOutcome,
) -> bool {
    matches!(
        outcome,
        harness_model_api::ModelTerminalOutcome::Failed(failure)
            if failure.kind == harness_model_api::ModelFailureKind::ProviderRejected
                && failure.message.contains("HTTP status 400")
                && failure
                    .message
                    .contains("unknown input item type: \\\"custom_tool_call\\\"")
    )
}

fn enqueue_runtime_effects(pending: &mut Vec<RuntimeEffect>, effects: Vec<RuntimeEffect>) {
    pending.extend(effects.into_iter().rev());
}

async fn emit_runtime_event(
    event_tx: &harness_runtime_api::RuntimeEventSender,
    seq: &mut u64,
    event: harness_runtime_api::RuntimeEvent,
) -> bool {
    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(*seq, event);
    *seq += 1;
    event_tx.send(envelope).await.is_ok()
}

async fn drive_runtime_effects(
    runtime: &mut ConversationRuntime,
    command_rx: &mut harness_runtime_api::RuntimeCommandReceiver,
    commands: &harness_runtime_api::RuntimeCommandSender,
    effects: Vec<RuntimeEffect>,
    event_tx: &harness_runtime_api::RuntimeEventSender,
    seq: &mut u64,
) -> bool {
    let mut pending = Vec::new();
    enqueue_runtime_effects(&mut pending, effects);

    while let Some(effect) = pending.pop() {
        let result = match effect {
            RuntimeEffect::Emit(event) => {
                let envelope = harness_runtime_api::RuntimeEventEnvelope::new(*seq, event);
                *seq += 1;
                if event_tx.send(envelope).await.is_err() {
                    return false;
                }
                Ok(Vec::new())
            }
            RuntimeEffect::StartModel {
                turn_id,
                attempt,
                route,
            } => {
                if !emit_runtime_event(
                    event_tx,
                    seq,
                    harness_runtime_api::RuntimeEvent::ModelAwaiting(true),
                )
                .await
                    || !emit_runtime_event(
                        event_tx,
                        seq,
                        harness_runtime_api::RuntimeEvent::SteeringChanged(None),
                    )
                    .await
                {
                    return false;
                }
                let result = run_model_attempt(runtime, command_rx, commands, turn_id, attempt, route, event_tx, seq).await;
                if !emit_runtime_event(
                    event_tx,
                    seq,
                    harness_runtime_api::RuntimeEvent::ModelAwaiting(false),
                )
                .await
                {
                    return false;
                }
                match result {
                    Ok(Some(effects)) => Ok(effects),
                    Ok(None) => return false,
                    Err(error) => Err((
                        harness_runtime_api::RuntimeFailureCategory::Model,
                        format!("starting model attempt failed: {error}"),
                    )),
                }
            },
            RuntimeEffect::StartCompaction {
                compaction_id: _,
                attempt,
                route,
            } => match run_model_attempt(runtime, command_rx, commands, 0, attempt, route, event_tx, seq).await {
                Ok(Some(effects)) => Ok(effects),
                Ok(None) => return false,
                Err(error) => Err((
                    harness_runtime_api::RuntimeFailureCategory::Model,
                    format!("starting compaction failed: {error}"),
                )),
            },
            RuntimeEffect::CommitCompaction {
                compaction_id,
                summary,
            } => runtime
                .commit_compaction(compaction_id, summary)
                .await
                .map_err(|error| {
                    (
                        harness_runtime_api::RuntimeFailureCategory::Session,
                        format!("compaction commit failed: {error}"),
                    )
                }),
            RuntimeEffect::CommitAssistant {
                turn_id,
                attempt_id,
            } => runtime
                .commit_assistant(turn_id, attempt_id)
                .await
                .map_err(|error| {
                    (
                        harness_runtime_api::RuntimeFailureCategory::Session,
                        format!("assistant commit failed: {error}"),
                    )
                }),
            RuntimeEffect::ContinueTurn { turn_id } => {
                runtime.continue_turn(turn_id).await.map_err(|error| {
                    (
                        harness_runtime_api::RuntimeFailureCategory::Lifecycle,
                        format!("turn continuation failed: {error}"),
                    )
                })
            }
            RuntimeEffect::ContinueModel { turn_id: _ } => runtime
                .start_model_request()
                .await
                .map(|effect| vec![effect])
                .map_err(|error| {
                    (
                        harness_runtime_api::RuntimeFailureCategory::Model,
                        format!("model continuation failed: {error}"),
                    )
                }),
            RuntimeEffect::ExecuteTool {
                turn_id,
                call_id,
                request,
            } => {
                let execution_id = request.execution_id.0;
                let output = match runtime.tool_executor() {
                    Ok(executor) => match executor.execute(request).await {
                        Ok(result) => result.model_output,
                        Err(error) => {
                            let message = match &error {
                                ToolFailure::InvalidInput(msg) => msg.clone(),
                                ToolFailure::Execution(msg) => msg.clone(),
                                ToolFailure::TimedOut => "The tool timed out before completing.".to_string(),
                                ToolFailure::Cancelled => "The tool was cancelled.".to_string(),
                            };
                            format!("The tool reported: {message}\n\nReview the error and retry the tool call with corrected input.")
                        },
                    },
                    Err(error) => {
                        return emit_effect_failure(
                            event_tx,
                            seq,
                            harness_runtime_api::RuntimeFailureCategory::Tool,
                            error.to_string(),
                        )
                        .await;
                    }
                };
                runtime
                    .commit_tool_result(turn_id, execution_id, call_id, output)
                    .await
                    .map_err(|error| {
                        (
                            harness_runtime_api::RuntimeFailureCategory::Tool,
                            format!("committing tool result failed: {error}"),
                        )
                    })
            }
            RuntimeEffect::LoadTranscriptPage {
                session_id,
                before_sequence,
                page_size,
            } => runtime
                .load_transcript_page(session_id, before_sequence, page_size)
                .map(|effect| vec![effect])
                .map_err(|error| {
                    (
                        harness_runtime_api::RuntimeFailureCategory::Session,
                        format!("loading transcript page failed: {error}"),
                    )
                }),
        };

        match result {
            Ok(next) => enqueue_runtime_effects(&mut pending, next),
            Err((category, message)) => {
                return emit_effect_failure(event_tx, seq, category, message).await;
            }
        }
    }

    true
}

async fn emit_effect_failure(
    event_tx: &harness_runtime_api::RuntimeEventSender,
    seq: &mut u64,
    category: harness_runtime_api::RuntimeFailureCategory,
    message: String,
) -> bool {
    emit_runtime_failure(event_tx, seq, category, message).await;
    true
}

fn start_conversation_runtime(
    session_id: harness_session_store::SessionId,
    model: ModelSelection,
    ports: RuntimePorts,
    provider_config: ProviderConfig,
    active_generation: Arc<AtomicU64>,
    session_root: PathBuf,
) -> RuntimeHandle {
    let (commands, events, event_tx, mut command_rx) = channel_pair(64);
    let commands_handle = commands.clone();

    tokio::spawn(async move {
        let mut runtime = ConversationRuntime::new(RuntimeConfiguration {
            session_id,
            model,
            ports,
        });
        let mut seq = 1;

        if let Err(e) = runtime.begin_startup().await {
            emit_runtime_failure(
                &event_tx,
                &mut seq,
                harness_runtime_api::RuntimeFailureCategory::Session,
                format!("runtime startup failed: {e}"),
            )
            .await;
            return;
        }
        if let Err(e) = runtime.mark_ready() {
            emit_runtime_failure(
                &event_tx,
                &mut seq,
                harness_runtime_api::RuntimeFailureCategory::Lifecycle,
                format!("runtime ready failed: {e}"),
            )
            .await;
            return;
        }

        let mut app_state = App {
            provider_config: provider_config.clone(),
            active_profile: runtime.active_model().provider.as_str().to_string(),
            session_id: runtime.session_id().as_str().to_string(),
        };
        let registry = match build_commands().map_err(|error| error.to_string()) {
            Ok(registry) => registry,
            Err(message) => {
                let message = format!("command registry initialization failed: {message}");
                emit_runtime_failure(
                    &event_tx,
                    &mut seq,
                    harness_runtime_api::RuntimeFailureCategory::Command,
                    message,
                )
                .await;
                return;
            }
        };

        while let Ok(cmd) = command_rx.recv().await {
            let is_shutdown = matches!(&cmd, harness_runtime_api::RuntimeCommand::Shutdown);

            if let harness_runtime_api::RuntimeCommand::SubmitPrompt { text } = &cmd
                && text.starts_with('/')
            {
                let result = match registry.dispatch(&mut app_state, text) {
                    Ok(Dispatch::Ran(action)) => {
                        execute_app_action(
                            action,
                            &mut app_state,
                            &provider_config,
                            &mut runtime,
                            &mut command_rx,
                            &commands,
                            &event_tx,
                            &session_root,
                            &active_generation,
                            &mut seq,
                            text,
                        )
                        .await
                    }
                    Ok(Dispatch::NotCommand) => Ok(()),
                    Err(error) => Err(error.to_string()),
                };

                if let Err(message) = result {
                    emit_runtime_failure(
                        &event_tx,
                        &mut seq,
                        harness_runtime_api::RuntimeFailureCategory::Command,
                        message,
                    )
                    .await;
                }
                continue;
            }

            let effects = match runtime.dispatch_command(cmd).await {
                Ok(effects) => effects,
                Err(error) => {
                    emit_runtime_failure(
                        &event_tx,
                        &mut seq,
                        harness_runtime_api::RuntimeFailureCategory::Command,
                        format!("runtime command failed: {error}"),
                    )
                    .await;
                    continue;
                }
            };

            if !drive_runtime_effects(&mut runtime, &mut command_rx, &commands, effects, &event_tx, &mut seq).await {
                break;
            }

            if is_shutdown {
                let mut shutdown_ok = true;

                if let Err(error) = runtime.join_jobs().await {
                    shutdown_ok = false;
                    emit_runtime_failure(
                        &event_tx,
                        &mut seq,
                        harness_runtime_api::RuntimeFailureCategory::Lifecycle,
                        format!("joining runtime jobs failed: {error}"),
                    )
                    .await;
                }
                if let Err(error) = runtime.shutdown_transport().await {
                    shutdown_ok = false;
                    emit_runtime_failure(
                        &event_tx,
                        &mut seq,
                        harness_runtime_api::RuntimeFailureCategory::Lifecycle,
                        format!("shutting down model transport failed: {error}"),
                    )
                    .await;
                }
                if let Err(error) = runtime.mark_stopped().await {
                    shutdown_ok = false;
                    emit_runtime_failure(
                        &event_tx,
                        &mut seq,
                        harness_runtime_api::RuntimeFailureCategory::Lifecycle,
                        format!("stopping runtime failed: {error}"),
                    )
                    .await;
                }

                if shutdown_ok {
                    if let Ok(event) = runtime.shutdown_complete_event() {
                        let envelope = harness_runtime_api::RuntimeEventEnvelope::new(seq, event);
                        let _ = event_tx.send(envelope).await;
                    }
                }
                break;
            }
        }
    });

    RuntimeHandle { commands: commands_handle, events }
}

async fn run_tui(_auth_mode: AuthMode, resume: ResumeSelection) -> CliResult<()> {
    let session_root = session_root()?;
    let startup = resolve_session_startup(&session_root, resume)?;

    let session_id = startup.session_id.clone();
    let session_id_text = session_id.as_str().to_string();

    let active_generation = Arc::new(AtomicU64::new(1));
    let workspace =
        WorkspaceRoot::open(env::current_dir().map_err(|source| CliError::CurrentDir { source })?)
            .map_err(|error| CliError::ToolRegistration(error.to_string()))?;
    let mut registry = ToolRegistry::new();
    let inventory = ToolInventory::register_into(&mut registry, workspace)
        .map_err(|error| CliError::ToolRegistration(error.to_string()))?;
    let tool_registry = Arc::new(RwLock::new(registry));
    let tool_availability = Arc::new(RwLock::new(ToolAvailability::new()));
    let inventory_executor: Arc<dyn ToolExecutor> = Arc::new(inventory);
    let tool_executor = Arc::new(AvailabilityToolExecutor::new(
        inventory_executor,
        Arc::clone(&tool_availability),
    ));

    let provider_config = load_provider_config().ok_or_else(|| {
        CliError::ProviderRuntime(ProviderError::Configuration(
            "provider configuration is required".to_string(),
        ))
    })?;

    // Try to restore a previously persisted provider selection so that
    // /provider use survives session restarts.
    let persisted = FileProviderSelectionStore {
        root: session_root.clone(),
    }
    .load()
    .await
    .unwrap_or(None);

    let (profile_id, model_slug, reasoning_effort, service_tier) =
        if let Some(selection) = &persisted {
            let profile_name = selection.provider.as_str();
            let pid = ProviderProfileId::new(profile_name);
            if provider_config.profile(&pid).is_some() {
                (
                    pid,
                    selection.model.model.clone(),
                    selection.model.reasoning_effort.clone(),
                    selection.model.service_tier.clone(),
                )
            } else {
                // Persisted profile no longer exists in config; fall back.
                let default_id = provider_config.default_profile_id.clone();
                let default_profile = provider_config.profile(&default_id).ok_or_else(|| {
                    CliError::ProviderRuntime(ProviderError::InvalidProfile(format!(
                        "default provider profile not found: {default_id}"
                    )))
                })?;
                (default_id, default_profile.default_model.clone(), None, None)
            }
        } else {
            let default_id = provider_config.default_profile_id.clone();
            let default_profile = provider_config.profile(&default_id).ok_or_else(|| {
                CliError::ProviderRuntime(ProviderError::InvalidProfile(format!(
                    "default provider profile not found: {default_id}"
                )))
            })?;
            (default_id, default_profile.default_model.clone(), None, None)
        };

    let (resolved_provider, initial_transport) = resolve_provider_and_transport(
        &provider_config,
        &session_id_text,
        &profile_id,
        &model_slug,
        1,
        reasoning_effort,
        service_tier,
    )
    .await?;

    let provider_state = ProviderState {
        display_name: ExternalText::new(resolved_provider.identity.display_name.clone()),
        kind: match resolved_provider.identity.id.as_str() {
            "codex" => ProviderKind::Codex,
            "ollama-cloud" => ProviderKind::OllamaCloud,
            _ => ProviderKind::HttpsApi,
        },
        transport: match resolved_provider.identity.transport.as_str() {
            "ws" => ProviderTransport::WebSocket,
            _ => ProviderTransport::Https,
        },
    };

    let model_state = ModelState {
        model: ExternalText::new(resolved_provider.selected_model.model.clone()),
        reasoning_effort: resolved_provider
            .selected_model
            .reasoning_effort
            .clone()
            .map(ExternalText::new),
        service_tier: resolved_provider
            .selected_model
            .service_tier
            .clone()
            .map(ExternalText::new),
    };

    let transcript = startup
        .initial_transcript_entries
        .into_iter()
        .map(harness_tui_rewrite::runtime::adapter::convert_snapshot_entry)
        .collect();

    let snapshot = InitialState {
        session_id: ExternalText::new(session_id_text.clone()),
        thread_title: ExternalText::new(format!(
            "new_harness · {}",
            resolved_provider.selected_model.model
        )),
        provider: Some(provider_state),
        model: model_state,
        developer_mode: false,
        response_streaming: false,
        last_ttft_ms: None,
        transcript,
        prompt: String::new(),
        prompt_cursor: 0,
        queued_steering: None,
        agents: Vec::new(),
        active_activity_ids: Vec::new(),
    };

    let ports = RuntimePorts {
        session_store: Arc::new(FileSessionStore {
            root: session_root.clone(),
        }),
        tool_registry: tool_registry
            .read()
            .map_err(|_| CliError::Io {
                source: io::Error::other("tool registry lock is poisoned"),
            })?
            .clone(),
        tool_executor: Some(tool_executor),
        model_transport: initial_transport.clone(),
        request_builder: Arc::new(RealModelRequestBuilder {
            tool_registry: Arc::clone(&tool_registry),
            tool_availability: Arc::clone(&tool_availability),
            base_instructions: load_base_instructions()?,
        }),
        model_route: resolved_provider.routes.root.clone(),
        compaction_route: resolved_provider.routes.compaction.clone(),
        tool_availability,
    };

    let runtime = start_conversation_runtime(
        session_id,
        resolved_provider.selected_model.clone(),
        ports,
        provider_config,
        active_generation,
        session_root,
    );

    harness_tui_rewrite::run_with_runtime(snapshot, runtime.commands, runtime.events).await?;

    let mut stdout = io::stdout();
    writeln!(stdout, "Resume conversation ID: {session_id_text}")?;
    stdout.flush()?;
    Ok(())
}

fn session_root() -> CliResult<PathBuf> {
    if let Ok(root) = env::var("HARNESS_SESSION_ROOT") {
        return Ok(PathBuf::from(root));
    }
    harness_state_dir()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    OwnState,
    CodexReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeSelection {
    New,
    Pick,
    SessionId(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliAction {
    Tui {
        auth_mode: AuthMode,
        resume: ResumeSelection,
    },
    IpcUds {
        socket_path: PathBuf,
    },
    RepairSession {
        session_id: String,
    },
    InspectSession {
        session_id: String,
    },
    ProbeSessionChunk {
        session_id: String,
        chunk_index: usize,
    },
}

#[derive(Debug)]
struct SessionStartup {
    session_id: harness_session_store::SessionId,
    initial_transcript_entries: Vec<TranscriptSnapshotEntry>,
}

fn resolve_session_startup(root: &Path, resume: ResumeSelection) -> CliResult<SessionStartup> {
    let is_new = matches!(&resume, ResumeSelection::New);
    let session_id = match resume {
        ResumeSelection::New => {
            let generated = generate_session_id()?;
            harness_session_store::SessionId::new(generated).map_err(|_| CliError::Io {
                source: io::Error::other("generated session ID is empty"),
            })?
        }
        ResumeSelection::SessionId(raw) => resolve_session_id(raw)?,
        ResumeSelection::Pick => latest_session_id(root)?,
    };

    let records = if is_new {
        Vec::new()
    } else {
        read_session_records(root, &session_id)?
    };
    let initial_transcript_entries = records
        .iter()
        .filter_map(transcript_snapshot_entry)
        .collect();

    Ok(SessionStartup {
        session_id,
        initial_transcript_entries,
    })
}

fn resolve_session_id(raw: String) -> CliResult<harness_session_store::SessionId> {
    if raw.is_empty() || raw == "." || raw == ".." || raw.contains('/') || raw.contains('\\') {
        return Err(CliError::SessionNotFound { id: raw });
    }
    harness_session_store::SessionId::new(raw.clone())
        .map_err(|_| CliError::SessionNotFound { id: raw })
}

fn latest_session_id(root: &Path) -> CliResult<harness_session_store::SessionId> {
    let directory = root.join("sessions");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CliError::NoSessionsAvailable);
        }
        Err(error) => return Err(CliError::Io { source: error }),
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| CliError::Io { source })?;
        if !entry
            .file_type()
            .map_err(|source| CliError::Io { source })?
            .is_file()
        {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(raw_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(session_id) = resolve_session_id(raw_id.to_owned()) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|source| CliError::Io { source })?;
        candidates.push((modified, session_id));
    }

    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.as_str().cmp(right.1.as_str()))
    });
    candidates
        .pop()
        .map(|(_, session_id)| session_id)
        .ok_or(CliError::NoSessionsAvailable)
}

fn read_session_records(
    root: &Path,
    session_id: &harness_session_store::SessionId,
) -> CliResult<Vec<SessionRecord>> {
    let path = root
        .join("sessions")
        .join(format!("{}.json", session_id.as_str()));
    if !path.exists() {
        return Err(CliError::SessionNotFound {
            id: session_id.as_str().to_owned(),
        });
    }
    let content = std::fs::read_to_string(&path).map_err(|source| CliError::Io { source })?;
    let loaded = serde_json::from_str::<Vec<SerializableRecord>>(&content).map_err(|error| {
        CliError::Session(harness_session_store::SessionStoreError::InvalidFormat(
            error.to_string(),
        ))
    })?;
    Ok(loaded
        .into_iter()
        .map(|record| from_serializable_record(record, session_id))
        .collect())
}

fn transcript_snapshot_entry(record: &SessionRecord) -> Option<TranscriptSnapshotEntry> {
    let payload = match &record.payload {
        SessionPayload::InputMessage { text, .. } => {
            harness_runtime_api::TranscriptPayload::Message {
                role: harness_runtime_api::MessageRole::User,
                text: text.clone(),
            }
        }
        SessionPayload::AssistantMessage { text, .. } => {
            harness_runtime_api::TranscriptPayload::Message {
                role: harness_runtime_api::MessageRole::Assistant,
                text: text.clone(),
            }
        }
        SessionPayload::Reasoning {
            content, summary, ..
        } => {
            let text = summary.as_deref().or(content.as_deref())?;
            if text.is_empty() {
                return None;
            }
            harness_runtime_api::TranscriptPayload::Thinking {
                text: text.to_owned(),
            }
        }
        SessionPayload::Error {
            category, message, ..
        } => harness_runtime_api::TranscriptPayload::Error {
            category: match category {
                harness_session_store::SessionErrorCategory::Model => {
                    harness_runtime_api::RuntimeFailureCategory::Model
                }
                harness_session_store::SessionErrorCategory::Protocol => {
                    harness_runtime_api::RuntimeFailureCategory::Protocol
                }
                harness_session_store::SessionErrorCategory::Tool => {
                    harness_runtime_api::RuntimeFailureCategory::Tool
                }
                harness_session_store::SessionErrorCategory::Lifecycle => {
                    harness_runtime_api::RuntimeFailureCategory::Lifecycle
                }
            },
            message: message.clone(),
        },
        SessionPayload::ToolExecutionFinished {
            call_id, output, ..
        } => harness_runtime_api::TranscriptPayload::ToolResult {
            call_id: call_id.clone(),
            output: output.clone(),
        },
        SessionPayload::CompactionCheckpoint { summary, .. } => {
            harness_runtime_api::TranscriptPayload::Thinking {
                text: summary.clone(),
            }
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
}

fn parse_cli_args(args: impl IntoIterator<Item = OsString>) -> CliResult<CliAction> {
    let mut auth_mode = AuthMode::OwnState;
    let mut positional = Vec::new();
    for arg in args {
        if arg == "--norotate" {
            auth_mode = AuthMode::CodexReadOnly;
            continue;
        }
        positional.push(arg.to_string_lossy().into_owned());
    }
    match positional.as_slice() {
        [] => Ok(CliAction::Tui {
            auth_mode,
            resume: ResumeSelection::New,
        }),
        [command] if command == "resume" => Ok(CliAction::Tui {
            auth_mode,
            resume: ResumeSelection::Pick,
        }),
        [command, session_id] if command == "resume" => Ok(CliAction::Tui {
            auth_mode,
            resume: ResumeSelection::SessionId(session_id.clone()),
        }),
        [command, session_id] if command == "repair-session" => Ok(CliAction::RepairSession {
            session_id: session_id.clone(),
        }),
        [command, session_id] if command == "inspect-session" => Ok(CliAction::InspectSession {
            session_id: session_id.clone(),
        }),
        [command, session_id, chunk_index] if command == "probe-session-chunk" => {
            Ok(CliAction::ProbeSessionChunk {
                session_id: session_id.clone(),
                chunk_index: chunk_index.parse().map_err(|source| CliError::Io {
                    source: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid chunk index `{chunk_index}`: {source}"),
                    ),
                })?,
            })
        }
        [command, socket_path] if command == "ipc-uds" => Ok(CliAction::IpcUds {
            socket_path: PathBuf::from(socket_path),
        }),
        _ => Err(CliError::UnsupportedArguments {
            arguments: positional.join(" "),
        }),
    }
}

fn harness_state_dir() -> CliResult<PathBuf> {
    if let Ok(root) = env::var("XDG_STATE_HOME")
        && !root.trim().is_empty()
    {
        return Ok(PathBuf::from(root).join("new_harness"));
    }
    let home = env::var("HOME").map_err(|source| CliError::HomeRequired {
        fallback_variable: "XDG_STATE_HOME",
        source,
    })?;
    Ok(PathBuf::from(home).join(".local/state/new_harness"))
}

fn load_base_instructions() -> CliResult<String> {
    let path = harness_state_dir()?.join("instructions.md");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(CliError::ReadBaseInstructions { path, source }),
    }
}

fn generate_session_id() -> CliResult<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CliError::Io {
            source: io::Error::other(format!("system clock before Unix epoch: {error}")),
        })?
        .as_nanos();
    let counter = NEXT_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bits = now ^ (u128::from(std::process::id()) << 64) ^ u128::from(counter);
    bits ^= bits.rotate_left(31);
    bits = bits.wrapping_mul(0x9e37_79b9_7f4a_7c15_d1b5_4a32_d192_ed03);
    Ok(format_uuid_like(bits))
}

fn format_uuid_like(mut bits: u128) -> String {
    bits &= !(0xf_u128 << 76);
    bits |= 0x4_u128 << 76;
    bits &= !(0x3_u128 << 62);
    bits |= 0x2_u128 << 62;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (bits >> 96) as u32,
        (bits >> 80) as u16,
        (bits >> 64) as u16,
        (bits >> 48) as u16,
        bits & 0xffff_ffff_ffff_u128
    )
}

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    configure_memory_allocator();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| CliError::RuntimeBuild { source })?;
    let result = runtime.block_on(run_cli());
    runtime.shutdown_timeout(CLI_RUNTIME_SHUTDOWN_TIMEOUT);
    result?;
    Ok(())
}

async fn run_cli() -> CliResult<()> {
    match parse_cli_args(env::args_os().skip(1))? {
        CliAction::Tui { auth_mode, resume } => run_tui(auth_mode, resume).await,
        CliAction::IpcUds { socket_path } => run_ipc_uds(socket_path).await,
        CliAction::RepairSession { session_id } => repair_session(session_id),
        CliAction::InspectSession { session_id } => inspect_session(session_id),
        CliAction::ProbeSessionChunk {
            session_id,
            chunk_index,
        } => probe_session_chunk(session_id, chunk_index),
    }
}

fn configure_memory_allocator() {
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
}

async fn run_ipc_uds(_socket_path: PathBuf) -> CliResult<()> {
    Err(harness_session_store::SessionStoreError::AdapterUnavailable.into())
}

fn repair_session(_session_id: String) -> CliResult<()> {
    Err(harness_session_store::SessionStoreError::AdapterUnavailable.into())
}

fn inspect_session(_session_id: String) -> CliResult<()> {
    Err(harness_session_store::SessionStoreError::AdapterUnavailable.into())
}

fn probe_session_chunk(_session_id: String, _chunk_index: usize) -> CliResult<()> {
    Err(harness_session_store::SessionStoreError::AdapterUnavailable.into())
}
