//! Command-line entrypoint for interactive harness sessions.

mod commands;
mod picker;

use std::{
    collections::VecDeque,
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
use harness_chat_completions_transport::{
    ChatCompletionsTransport, ChatStreamChunk, ChatStreamError, ChatStreamingClient,
    ChatTransportConfiguration,
};
use harness_conversation_runtime::{
    ConversationRuntime, ModelRequestBuilder, RuntimeConfiguration, RuntimeEffect, RuntimeError,
    RuntimePorts,
};
use harness_model_api::{
    ModelAttempt, ModelAttemptHandle, ModelFailure, ModelInput, ModelMessageRole, ModelRequest,
    ModelRequestId, ModelSelection, ModelTransport, ProviderGeneration, RequestContextUsage,
    ResolvedModelRoute,
};
use harness_provider::{
    ProviderAuthConfig, ProviderConfig, ProviderDriverConfig, ProviderError, ProviderIdentity,
    ProviderProfileId, ProviderRoutes, ProviderSelection, ProviderSelectionStore, ResolvedProvider,
};
use harness_responses_api::{
    ApiProvider, Auth, AuthError, ChatGptAuthTokens, CodexHeaders, ManagedChatGptAuth,
    ResponsesApiError, ResponsesStreamEvent, ResponsesStreamRequest, is_auto_retryable_http_status,
    lean_codex_default_headers,
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
    SessionPayload, SessionReader, SessionRecord, SessionStore, SessionStoreError,
    SessionToolInput, SessionWriter,
};
use harness_tool_api::{AvailabilityToolExecutor, ToolAvailability, ToolExecutor, ToolRegistry};
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
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(crate) struct SerializableRecord {
    pub(crate) sequence: u64,
    pub(crate) payload: SerializablePayload,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(crate) enum SerializablePayload {
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
        invocation: harness_tool_api::ToolInvocation,
        raw_input: SerializableToolInput,
    },
    ToolExecutionFinished {
        turn_id: u64,
        call_id: String,
        invocation: harness_tool_api::ToolInvocation,
        outcome: harness_tool_api::ToolOutcome,
        raw_input: SerializableToolInput,
        raw_output: String,
    },
    TurnFinished {
        turn_id: u64,
        outcome: SerializableTurnOutcome,
    },
    CompactionCheckpoint {
        source_revision: u64,
        summary: String,
    },
    ModelResponseMetadata {
        turn_id: u64,
        attempt_id: u64,
        provider: String,
        response_id: String,
    },
    PreviousResponseInvalidated {
        turn_id: u64,
    },
    Goal {
        instruction: String,
        state: String,
    },
    SessionClosed,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(untagged)]
pub(crate) enum SerializableToolInput {
    Encoded(SerializableEncodedToolInput),
    Unspecified(String),
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(crate) enum SerializableEncodedToolInput {
    Freeform(String),
    FunctionJson(String),
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
            invocation,
            raw_input,
        } => SerializablePayload::ToolCallAccepted {
            turn_id: *turn_id,
            call_id: call_id.clone(),
            invocation: invocation.clone(),
            raw_input: serialize_tool_input(raw_input),
        },
        SessionPayload::ToolExecutionFinished {
            turn_id,
            call_id,
            invocation,
            outcome,
            raw_input,
            raw_output,
        } => SerializablePayload::ToolExecutionFinished {
            turn_id: *turn_id,
            call_id: call_id.clone(),
            invocation: invocation.clone(),
            outcome: outcome.clone(),
            raw_input: serialize_tool_input(raw_input),
            raw_output: raw_output.clone(),
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
        SessionPayload::ModelResponseMetadata {
            turn_id,
            attempt_id,
            provider,
            response_id,
        } => SerializablePayload::ModelResponseMetadata {
            turn_id: *turn_id,
            attempt_id: *attempt_id,
            provider: provider.clone(),
            response_id: response_id.clone(),
        },
        SessionPayload::PreviousResponseInvalidated { turn_id } => {
            SerializablePayload::PreviousResponseInvalidated { turn_id: *turn_id }
        }
        SessionPayload::Goal { instruction, state } => SerializablePayload::Goal {
            instruction: instruction.clone(),
            state: state.clone(),
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
            invocation,
            raw_input,
        } => SessionPayload::ToolCallAccepted {
            turn_id,
            call_id,
            invocation,
            raw_input: deserialize_tool_input(raw_input),
        },
        SerializablePayload::ToolExecutionFinished {
            turn_id,
            call_id,
            invocation,
            outcome,
            raw_input,
            raw_output,
        } => SessionPayload::ToolExecutionFinished {
            turn_id,
            call_id,
            invocation,
            outcome,
            raw_input: deserialize_tool_input(raw_input),
            raw_output,
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
        SerializablePayload::ModelResponseMetadata {
            turn_id,
            attempt_id,
            provider,
            response_id,
        } => SessionPayload::ModelResponseMetadata {
            turn_id,
            attempt_id,
            provider,
            response_id,
        },
        SerializablePayload::PreviousResponseInvalidated { turn_id } => {
            SessionPayload::PreviousResponseInvalidated { turn_id }
        }
        SerializablePayload::Goal { instruction, state } => {
            SessionPayload::Goal { instruction, state }
        }
        SerializablePayload::SessionClosed => SessionPayload::SessionClosed,
    }
}

fn to_serializable_record(record: &SessionRecord) -> SerializableRecord {
    SerializableRecord {
        sequence: record.sequence,
        payload: to_serializable_payload(&record.payload),
    }
}

fn serialize_tool_input(input: &SessionToolInput) -> SerializableToolInput {
    match input {
        SessionToolInput::Freeform(input) => {
            SerializableToolInput::Encoded(SerializableEncodedToolInput::Freeform(input.clone()))
        }
        SessionToolInput::FunctionJson(input) => SerializableToolInput::Encoded(
            SerializableEncodedToolInput::FunctionJson(input.clone()),
        ),
        SessionToolInput::Unspecified(input) => {
            SerializableToolInput::Unspecified(input.clone())
        }
    }
}

fn deserialize_tool_input(input: SerializableToolInput) -> SessionToolInput {
    match input {
        SerializableToolInput::Encoded(SerializableEncodedToolInput::Freeform(input)) => {
            SessionToolInput::Freeform(input)
        }
        SerializableToolInput::Encoded(SerializableEncodedToolInput::FunctionJson(input)) => {
            SessionToolInput::FunctionJson(input)
        }
        SerializableToolInput::Unspecified(input) => SessionToolInput::Unspecified(input),
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

fn deserialize_session_records(content: &str) -> Result<Vec<SerializableRecord>, SessionStoreError> {
    let mut records = serde_json::from_str::<Vec<serde_json::Value>>(content)
        .map_err(|error| SessionStoreError::InvalidFormat(error.to_string()))?;
    upgrade_legacy_session_records(&mut records)?;
    serde_json::from_value(serde_json::Value::Array(records))
        .map_err(|error| SessionStoreError::InvalidFormat(error.to_string()))
}

fn upgrade_legacy_session_records(
    records: &mut Vec<serde_json::Value>,
) -> Result<(), SessionStoreError> {
    use harness_tool_api::{BuiltInTool, ToolExecutionFailure, ToolFailureCategory, ToolInvocation, ToolOutcome};

    let mut accepted = std::collections::BTreeMap::new();
    records.retain_mut(|record| {
        let Some(payload) = record.get_mut("payload").and_then(serde_json::Value::as_object_mut)
        else {
            return true;
        };

        if payload.contains_key("ToolExecutionStarted") {
            return false;
        }

        if let Some(fields) = payload
            .get_mut("ToolCallAccepted")
            .and_then(serde_json::Value::as_object_mut)
        {
            let turn_id = fields.get("turn_id").and_then(serde_json::Value::as_u64);
            let call_id = fields
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            if !fields.contains_key("invocation") {
                let Some(name) = fields.remove("name").and_then(|value| value.as_str().map(str::to_owned)) else {
                    return true;
                };
                let Some(tool) = BuiltInTool::from_name(&name) else {
                    return true;
                };
                let invocation = ToolInvocation::rejected(
                    tool,
                    "loaded from a legacy session without parsed invocation data".to_string(),
                );
                fields.insert("invocation".to_string(), serde_json::to_value(&invocation).unwrap());
                if let Some(input) = fields.remove("input") {
                    fields.insert("raw_input".to_string(), input);
                }
            }
            if let (Some(turn_id), Some(call_id), Some(invocation), Some(raw_input)) = (
                turn_id,
                call_id,
                fields.get("invocation").cloned(),
                fields.get("raw_input").cloned(),
            ) {
                accepted.insert((turn_id, call_id), (invocation, raw_input));
            }
        }

        if let Some(fields) = payload
            .get_mut("ToolExecutionFinished")
            .and_then(serde_json::Value::as_object_mut)
        {
            if !fields.contains_key("invocation") {
                let key = fields
                    .get("turn_id")
                    .and_then(serde_json::Value::as_u64)
                    .zip(fields.get("call_id").and_then(serde_json::Value::as_str))
                    .map(|(turn_id, call_id)| (turn_id, call_id.to_owned()));
                if let Some((invocation, raw_input)) = key.and_then(|key| accepted.get(&key).cloned()) {
                    fields.insert("invocation".to_string(), invocation);
                    fields.insert("raw_input".to_string(), raw_input);
                    let raw_output = fields.remove("output").unwrap_or(serde_json::Value::String(String::new()));
                    fields.insert("raw_output".to_string(), raw_output);
                    let outcome = ToolOutcome::Failed(ToolExecutionFailure {
                        category: ToolFailureCategory::Execution,
                        message: "legacy session contains no structured tool outcome".to_string(),
                    });
                    fields.insert("outcome".to_string(), serde_json::to_value(outcome).unwrap());
                }
            }
        }
        true
    });
    Ok(())
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
                let loaded = deserialize_session_records(&content)?;
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
        let loaded = deserialize_session_records(&content)?;

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
    provider_config: ProviderConfig,
    base_instructions: String,
    store: bool,
}

impl RealModelRequestBuilder {
    fn estimate_context(
        &self,
        selection: &ModelSelection,
        history: &[SessionPayload],
        steering: &[String],
        tools: &[harness_tool_api::ToolDefinition],
    ) -> Option<RequestContextUsage> {
        let profile = self
            .provider_config
            .profile(&ProviderProfileId::new(selection.provider.as_str()))?;
        let model = profile
            .model_configs
            .iter()
            .find(|model| model.slug == selection.model)?;
        let tokenizer = if selection.model.starts_with("gpt-5")
            || selection.model.starts_with("gpt-4o")
            || selection.model.starts_with("o1")
            || selection.model.starts_with("o3")
            || selection.model.starts_with("o4")
        {
            tiktoken_rs::o200k_base_singleton()
        } else {
            tiktoken_rs::get_bpe_from_model(selection.model.as_str()).ok()?
        };
        let mut tokens = 0usize;
        let mut add =
            |value: &str| tokens = tokens.saturating_add(tokenizer.encode_ordinary(value).len());

        if !self.base_instructions.is_empty() {
            add(&self.base_instructions);
        }
        for payload in history {
            match payload {
                SessionPayload::InputMessage { text, .. }
                | SessionPayload::AssistantMessage { text, .. } => add(text),
                SessionPayload::Reasoning {
                    content,
                    encrypted_content,
                    summary,
                    ..
                } => {
                    for value in [content, encrypted_content, summary].into_iter().flatten() {
                        add(value);
                    }
                }
                SessionPayload::CompactionCheckpoint { summary, .. } => add(summary),
                SessionPayload::ToolCallAccepted {
                    call_id,
                    invocation,
                    raw_input,
                    ..
                } => {
                    add(call_id);
                    add(invocation.tool().name());
                    add(raw_input.as_str());
                }
                SessionPayload::ToolExecutionFinished {
                    call_id,
                    raw_output,
                    ..
                } => {
                    add(call_id);
                    add(raw_output);
                }
                _ => {}
            }
        }
        for value in steering {
            add(value);
        }
        for tool in tools {
            add(tool.name.as_str());
            add(&tool.description);
            match &tool.input_schema {
                harness_tool_api::ToolInputSchema::FreeformGrammar { definition, .. } => {
                    add(definition)
                }
                harness_tool_api::ToolInputSchema::JsonSchema(schema) => add(schema.as_str()),
            }
        }

        Some(RequestContextUsage {
            estimated_input_tokens: u64::try_from(tokens).unwrap_or(u64::MAX),
            max_input_tokens: model.context_window,
            compact_at_tokens: (model.context_window * model.effective_context_window_percent)
                / 100,
        })
    }
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
        let uses_freeform_encoding = |name: &str, input: &SessionToolInput| match input {
            SessionToolInput::Freeform(_) => true,
            SessionToolInput::FunctionJson(_) => false,
            SessionToolInput::Unspecified(_) => registry.iter().any(|(_, tool)| {
                tool.definition.name.as_str() == name
                    && matches!(
                        &tool.definition.input_schema,
                        harness_tool_api::ToolInputSchema::FreeformGrammar { .. }
                    )
            }),
        };

        let last_response_boundary = history.iter().rposition(|payload| {
            matches!(
                payload,
                SessionPayload::CompactionCheckpoint { .. }
                    | SessionPayload::PreviousResponseInvalidated { .. }
            )
        });
        let last_response_meta = if self.store {
            history.iter().enumerate().rev().find_map(|(idx, payload)| {
                if last_response_boundary.is_some_and(|boundary| idx < boundary) {
                    return None;
                }
                if let SessionPayload::ModelResponseMetadata {
                    provider,
                    response_id,
                    ..
                } = payload
                {
                    if provider == selection.provider.as_str() {
                        return Some((idx, response_id.clone()));
                    }
                }
                None
            })
        } else {
            None
        };
        let last_compaction_pos = history
            .iter()
            .rposition(|payload| matches!(payload, SessionPayload::CompactionCheckpoint { .. }));

        let (history_slice, previous_response_id) = match (last_response_meta, last_compaction_pos)
        {
            (Some((meta_idx, response_id)), Some(comp_idx)) if meta_idx > comp_idx => {
                (&history[meta_idx + 1..], Some(response_id))
            }
            (_, Some(comp_idx)) => (&history[comp_idx..], None),
            (Some((meta_idx, response_id)), None) => (&history[meta_idx + 1..], Some(response_id)),
            (None, None) => (history, None),
        };

        let mut input = Vec::new();
        if previous_response_id.is_none() && !self.base_instructions.is_empty() {
            input.push(ModelInput::Message {
                role: ModelMessageRole::System,
                text: self.base_instructions.clone(),
            });
        }
        let mut call_is_in_previous_response = previous_response_id.is_some();
        for payload in history_slice {
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
                SessionPayload::ModelAttemptStarted { .. } => {
                    call_is_in_previous_response = false;
                }
                SessionPayload::ToolCallAccepted {
                    call_id,
                    invocation,
                    raw_input,
                    ..
                } => {
                    if call_is_in_previous_response {
                        continue;
                    }
                    let name = invocation.tool().name();
                    if uses_freeform_encoding(name, raw_input) {
                        input.push(ModelInput::FreeformToolCall {
                            call_id: call_id.clone(),
                            name: name.to_owned(),
                            input: raw_input.as_str().to_owned(),
                        });
                    } else {
                        input.push(ModelInput::AssistantToolCall {
                            call_id: call_id.clone(),
                            name: name.to_owned(),
                            arguments: raw_input.as_str().to_owned(),
                        });
                    }
                }
                SessionPayload::ToolExecutionFinished {
                    call_id,
                    invocation,
                    raw_input,
                    raw_output,
                    ..
                } => {
                    if uses_freeform_encoding(invocation.tool().name(), raw_input) {
                        input.push(ModelInput::FreeformToolResult {
                            call_id: call_id.clone(),
                            output: raw_output.clone(),
                        });
                    } else {
                        input.push(ModelInput::ToolResult {
                            call_id: call_id.clone(),
                            output: raw_output.clone(),
                        });
                    }
                }
                SessionPayload::PreviousResponseInvalidated { .. } => {}
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

        let context_usage = self.estimate_context(selection, history, steering, &tools);
        Ok(Arc::new(ModelRequest {
            request_id: ModelRequestId(0),
            provider_generation,
            history_revision: revision,
            selection: selection.clone(),
            input: Arc::from(input),
            tools: Arc::from(tools),
            previous_response_id,
            context_usage,
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
        let request = self.build(revision, selection, provider_generation, history, steering)?;
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
                if matches!(
                    &tool.input_schema,
                    harness_tool_api::ToolInputSchema::FreeformGrammar { .. }
                ) {
                    tool.description = tool.function_compatibility_description();
                    let schema = sonic_rs::json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Complete raw tool input, passed as one JSON string exactly as specified by the function description."
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
        let context_usage = self.estimate_context(selection, history, steering, &tools);
        Ok(Arc::new(ModelRequest {
            input: Arc::from(input),
            tools: Arc::from(tools),
            context_usage,
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
    ) -> Pin<Box<dyn Future<Output = Result<Option<ProviderSelection>, ProviderError>> + Send + '_>>
    {
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
            let ser: SerializableProviderSelection = serde_json::from_str(&data).map_err(|e| {
                ProviderError::Persistence(format!("invalid provider-bound.json: {e}"))
            })?;
            let provider = harness_model_api::ProviderId::new(ser.provider).map_err(|e| {
                ProviderError::Persistence(format!(
                    "invalid provider ID in persisted selection: {e}"
                ))
            })?;
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
    capabilities: harness_model_api::ModelCapabilities,
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
        let developer_role_support = self.capabilities.developer_role_support;
        let allow_multiple_system = self.capabilities.allow_multiple_system_messages;
        let store = self.capabilities.store;
        headers.client_request_id = attempt.attempt_id.0.to_string();
        headers.thread_id = attempt.attempt_id.0.to_string();
        Box::pin(async move {
            let encoded_input = encode_input(
                &attempt.request.input,
                developer_role_support,
                allow_multiple_system,
            );
            let tools = encode_tools(&attempt.request.tools)
                .map_err(|error| StreamError::Transport(error.to_string()))?;
            let mut body = sonic_rs::json!({
                "type": "response.create",
                "model": attempt.request.selection.model,
                "input": encoded_input,
                "tools": tools,
                "store": store,
            });
            if store {
                if let Some(prev_id) = &attempt.request.previous_response_id {
                    body["previous_response_id"] = sonic_rs::json!(prev_id);
                }
            }
            add_selection_options(&mut body, &attempt.request.selection, &headers.session_id);
            let request = ResponsesStreamRequest { headers, body };
            let (sender, receiver) = tokio::sync::mpsc::channel(128);
            tokio::spawn(async move {
                let result = pool
                    .stream_request(request, |event| {
                        let sender = sender.clone();
                        async move {
                            let chunk = match event {
                                ResponsesStreamEvent::Frame(value) => {
                                    match sonic_rs::to_vec(&value) {
                                        Ok(mut bytes) => {
                                            let mut sse_bytes = Vec::with_capacity(bytes.len() + 8);
                                            sse_bytes.extend_from_slice(b"data: ");
                                            sse_bytes.append(&mut bytes);
                                            sse_bytes.extend_from_slice(b"\n\n");
                                            Ok(StreamChunk::Bytes(sse_bytes))
                                        }
                                        Err(error) => {
                                            Err(StreamError::Transport(error.to_string()))
                                        }
                                    }
                                }
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
                    let err_msg = format_input_index_error(&error.to_string(), &encoded_input);
                    let _ = sender.send(Err(StreamError::Transport(err_msg))).await;
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
    capabilities: harness_model_api::ModelCapabilities,
    hyper_client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::Full<bytes::Bytes>,
    >,
}

impl HttpClient {
    fn new(
        base_url: String,
        api_key: Option<String>,
        session_id: impl Into<String>,
        capabilities: harness_model_api::ModelCapabilities,
    ) -> Self {
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
            capabilities,
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
            let body_bytes = bytes::Bytes::from(
                sonic_rs::to_vec(&body)
                    .map_err(|e| ChatStreamError::Transport(e.to_string()))?,
            );

            loop {
                let mut req =
                    http::Request::post(&url_str).header("content-type", "application/json");

                if let Some(key) = &api_key {
                    req = req.header("authorization", format!("Bearer {key}"));
                }

                let req = req
                    .body(http_body_util::Full::new(body_bytes.clone()))
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
                    if is_auto_retryable_http_status(status) {
                        continue;
                    }
                    if status == http::StatusCode::UNAUTHORIZED {
                        return Err(ChatStreamError::Authentication(err_msg));
                    }
                    let formatted_err = if err_msg.trim().is_empty() {
                        format!("HTTP status {status}")
                    } else {
                        format!("HTTP status {status}: {err_msg}")
                    };
                    return Err(ChatStreamError::ProviderRejected(formatted_err));
                }

                let body = resp.into_body();
                let stream = futures_util::stream::unfold(Some(body), |state| async move {
                    let mut body = state?;

                    match body.frame().await {
                        Some(Ok(frame)) => {
                            if let Some(data) = frame.data_ref()
                                && !data.is_empty()
                            {
                                return Some((
                                    Ok(ChatStreamChunk::Bytes(data.to_vec())),
                                    Some(body),
                                ));
                            }
                            Some((Ok(ChatStreamChunk::Bytes(Vec::new())), Some(body)))
                        }
                        Some(Err(err)) => {
                            Some((Err(ChatStreamError::Transport(err.to_string())), None))
                        }
                        None => Some((Ok(ChatStreamChunk::End), None)),
                    }
                });
                return Ok(Box::pin(stream)
                    as Pin<
                        Box<dyn Stream<Item = Result<ChatStreamChunk, ChatStreamError>> + Send>,
                    >);
            }
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
        let developer_role_support = self.capabilities.developer_role_support;
        let allow_multiple_system = self.capabilities.allow_multiple_system_messages;
        let store = self.capabilities.store;
        let hyper_client = self.hyper_client.clone();

        Box::pin(async move {
            let url_str = format!("{}/responses", base_url.trim_end_matches('/'));

            let input = encode_input(
                &attempt.request.input,
                developer_role_support,
                allow_multiple_system,
            );
            let tools = encode_tools(&attempt.request.tools)
                .map_err(|error| StreamError::Transport(error.to_string()))?;
            let mut body = sonic_rs::json!({
                "model": attempt.request.selection.model,
                "input": input,
                "tools": tools,
                "stream": true,
                "store": store,
            });
            if store {
                if let Some(prev_id) = &attempt.request.previous_response_id {
                    body["previous_response_id"] = sonic_rs::json!(prev_id);
                }
            }
            add_selection_options(&mut body, &attempt.request.selection, &session_id);
            let body_bytes = bytes::Bytes::from(
                sonic_rs::to_vec(&body).map_err(|e| StreamError::Transport(e.to_string()))?,
            );

            loop {
                let mut req =
                    http::Request::post(&url_str).header("content-type", "application/json");

                if let Some(key) = &api_key {
                    req = req.header("authorization", format!("Bearer {key}"));
                }

                let req = req
                    .body(http_body_util::Full::new(body_bytes.clone()))
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
                    if is_auto_retryable_http_status(status) {
                        continue;
                    }
                    let err_msg = String::from_utf8_lossy(&err_bytes);
                    let formatted_err = format!("HTTP status {status}: {err_msg}");
                    let detailed_err = format_input_index_error(&formatted_err, &input);
                    return Err(StreamError::Transport(detailed_err));
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
                        Some(Err(err)) => {
                            Some((Err(StreamError::Transport(err.to_string())), None))
                        }
                        None => Some((Ok(StreamChunk::End), None)),
                    }
                });
                return Ok(Box::pin(stream)
                    as Pin<Box<dyn Stream<Item = Result<StreamChunk, StreamError>> + Send>>);
            }
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
    #[error("failed to encode experiment JSON")]
    ExperimentJson {
        #[source]
        source: sonic_rs::Error,
    },
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
    #[error("no session selected")]
    NoSessionSelected,
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
    #[error("failed to read harness base instructions {path}")]
    ReadBaseInstructions {
        path: PathBuf,
        #[source]
        source: io::Error,
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

fn format_input_index_error(err_msg: &str, encoded_input: &[sonic_rs::Value]) -> String {
    if let Some(pos) = err_msg.find("input[") {
        let rest = &err_msg[pos + 6..];
        if let Some(end_pos) = rest.find(']') {
            if let Ok(idx) = rest[..end_pos].parse::<usize>() {
                if let Some(item) = encoded_input.get(idx) {
                    let item_str =
                        sonic_rs::to_string_pretty(item).unwrap_or_else(|_| item.to_string());
                    return format!(
                        "{err_msg}\n\n--> Target input[{idx}] causing error:\n{item_str}"
                    );
                }
            }
        }
    }
    err_msg.to_string()
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

    let dev_role_support = match model_config.developer_role_support {
        Some(harness_provider::DeveloperRoleSupport::Disabled) => {
            harness_model_api::DeveloperRoleSupport::Disabled
        }
        Some(harness_provider::DeveloperRoleSupport::Supported) => {
            harness_model_api::DeveloperRoleSupport::Supported
        }
        Some(harness_provider::DeveloperRoleSupport::DeveloperOnly) => {
            harness_model_api::DeveloperRoleSupport::DeveloperOnly
        }
        None => match profile.developer_role_support {
            harness_provider::DeveloperRoleSupport::Disabled => {
                harness_model_api::DeveloperRoleSupport::Disabled
            }
            harness_provider::DeveloperRoleSupport::Supported => {
                harness_model_api::DeveloperRoleSupport::Supported
            }
            harness_provider::DeveloperRoleSupport::DeveloperOnly => {
                harness_model_api::DeveloperRoleSupport::DeveloperOnly
            }
        },
    };

    let allow_multiple_system = model_config
        .allow_multiple_system_messages
        .unwrap_or(profile.allow_multiple_system_messages);

    let store = model_config.store.unwrap_or(profile.store);

    let capabilities = harness_model_api::ModelCapabilities {
        tool_calls: model_config.supports_tools,
        freeform_tool_input: profile.supports_freeform_tool_input.unwrap_or(!matches!(
            &profile.driver,
            ProviderDriverConfig::ChatCompletion { .. }
        )),
        streaming: true,
        developer_role_support: dev_role_support,
        allow_multiple_system_messages: allow_multiple_system,
        store,
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
            let client = Arc::new(HttpClient::new(
                base_url.clone(),
                api_key,
                session_id,
                capabilities,
            ));
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
            let client = Arc::new(HttpClient::new(
                base_url.clone(),
                api_key,
                session_id,
                capabilities,
            ));
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
                capabilities,
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

async fn emit_notification(
    text: &str,
    event_tx: &harness_runtime_api::RuntimeEventSender,
    seq: &mut u64,
) {
    if text.is_empty() {
        return;
    }
    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
        *seq,
        harness_runtime_api::RuntimeEvent::TranscriptAppended(TranscriptSnapshotEntry {
            sequence: None,
            payload: harness_runtime_api::TranscriptPayload::Message {
                role: harness_runtime_api::MessageRole::Assistant,
                text: text.to_string(),
            },
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
    let mut payloads = Vec::new();
    let has_user = !user_text.is_empty();
    if has_user {
        payloads.push(SessionPayload::InputMessage {
            turn_id: 0,
            text: user_text.to_string(),
        });
    }
    let has_assistant = !assistant_text.is_empty();
    if has_assistant {
        payloads.push(SessionPayload::AssistantMessage {
            turn_id: 0,
            text: assistant_text.to_string(),
        });
    }

    if payloads.is_empty() {
        return;
    }

    let receipt = runtime.append_records(&payloads).await;
    let (user_seq, assistant_seq) = match &receipt {
        Ok(rcpt) => {
            let start = *rcpt.sequences.start();
            let end = *rcpt.sequences.end();
            if has_user && has_assistant {
                (Some(start), Some(end))
            } else if has_user {
                (Some(start), None)
            } else {
                (None, Some(start))
            }
        }
        Err(_) => (None, None),
    };

    if has_user {
        let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
            *seq,
            harness_runtime_api::RuntimeEvent::TranscriptAppended(TranscriptSnapshotEntry {
                sequence: user_seq,
                payload: harness_runtime_api::TranscriptPayload::Message {
                    role: harness_runtime_api::MessageRole::User,
                    text: user_text.to_string(),
                },
            }),
        );
        *seq += 1;
        let _ = event_tx.send(envelope).await;
    }

    if has_assistant {
        let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
            *seq,
            harness_runtime_api::RuntimeEvent::TranscriptAppended(TranscriptSnapshotEntry {
                sequence: assistant_seq,
                payload: harness_runtime_api::TranscriptPayload::Message {
                    role: harness_runtime_api::MessageRole::Assistant,
                    text: assistant_text.to_string(),
                },
            }),
        );
        *seq += 1;
        let _ = event_tx.send(envelope).await;
    }
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
    SetToolAvailability {
        pattern: String,
        enabled: bool,
    },
    SetGoal {
        instruction: String,
    },
    PauseGoal,
    ResumeGoal,
    ClearGoal,
    ShowGoal,
    Compact {
        instruction: String,
    },
    RetryCompaction {
        instruction: Option<String>,
    },
    CancelCompaction,
    ResumeSession {
        session_id: Option<String>,
    },
    /// Fork current session up to an optional sequence cutpoint and switch to the fork.
    /// `before_compaction` rewinds to just before the last CompactionCheckpoint.
    Rewind {
        /// Rewind to just before the last compaction checkpoint.
        before_compaction: bool,
        /// Rewind to before this specific sequence number, if provided.
        before_sequence: Option<u64>,
    },
    EditMessage(EditMessageAction),
}
#[derive(Debug, PartialEq, Eq)]
enum EditMessageAction {
    Choose,
    Replace { sequence: u64, text: String },
    Delete { sequence: u64 },
}

type Commands = CommandRegistry<App, AppAction>;
type Context<'a> = CommandContext<'a, App, AppAction>;

fn provider(app: &mut App, mut context: Context<'_>) -> CommandResult<AppAction> {
    let subcmd = context.args.next();
    match subcmd {
        None | Some("list") => {
            let mut list = String::from("Available provider profiles:\n");
            for profile in &app.provider_config.profiles {
                let is_active = profile.id.as_str() == app.active_profile.as_str();
                let marker = if is_active { " (active)" } else { "" };
                let _ = writeln!(
                    list,
                    "  • {}{} — {}",
                    profile.id.as_str(),
                    marker,
                    profile.display_name
                );
            }
            Ok(AppAction::ShowMessage(list))
        }
        Some("use") => {
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
        Some(profile_name) => {
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

fn resume(_app: &mut App, mut context: Context<'_>) -> CommandResult<AppAction> {
    let session_id = context.args.next().map(str::to_owned);
    context.args.finish()?;
    Ok(AppAction::ResumeSession { session_id })
}

fn rewind(_app: &mut App, mut context: Context<'_>) -> CommandResult<AppAction> {
    let mut before_compaction = false;
    let mut before_sequence: Option<u64> = None;
    for arg in &mut context.args {
        if arg == "compact" || arg == "compaction" {
            before_compaction = true;
        } else if let Some(seq_str) = arg.strip_prefix("before:") {
            before_sequence = Some(seq_str.parse().map_err(|_| {
                CommandError::Message(format!("invalid sequence number: {seq_str}").into())
            })?);
        } else {
            return Err(CommandError::Message(
                format!("unknown rewind argument: {arg}").into(),
            ));
        }
    }
    Ok(AppAction::Rewind {
        before_compaction,
        before_sequence,
    })
}

fn edit_message(_app: &mut App, context: Context<'_>) -> CommandResult<AppAction> {
    parse_edit_message_action(context.raw_args).map(AppAction::EditMessage)
}

fn parse_edit_message_action(raw_args: &str) -> CommandResult<EditMessageAction> {
    if raw_args.is_empty() {
        return Ok(EditMessageAction::Choose);
    }

    let mut words = raw_args.split_whitespace();
    if words.next() == Some("delete") {
        let sequence_text = words
            .next()
            .ok_or(CommandError::MissingArgument { name: "sequence" })?;
        let sequence = sequence_text
            .parse()
            .map_err(|_| CommandError::InvalidArgument {
                name: "sequence",
                value: sequence_text.into(),
                reason: "expected a persisted transcript sequence number".into(),
            })?;
        if let Some(extra) = words.next() {
            return Err(CommandError::UnexpectedArgument(extra.into()));
        }
        return Ok(EditMessageAction::Delete { sequence });
    }

    let Some(separator) = raw_args.find(char::is_whitespace) else {
        return Err(CommandError::Message(
            "missing replacement text; use `/edit <sequence> <text>` to edit or \
             `/edit delete <sequence>` to remove the message"
                .into(),
        ));
    };
    let sequence_text = &raw_args[..separator];
    let sequence = sequence_text
        .parse()
        .map_err(|_| CommandError::InvalidArgument {
            name: "sequence",
            value: sequence_text.into(),
            reason: "expected a persisted transcript sequence number".into(),
        })?;
    let separator_len = raw_args[separator..]
        .chars()
        .next()
        .map(char::len_utf8)
        .unwrap_or(0);
    let text = &raw_args[separator + separator_len..];
    if text.is_empty() {
        return Err(CommandError::Message(
            "replacement text is empty; provide message text or use \
             `/edit delete <sequence>` to remove the message"
                .into(),
        ));
    }

    Ok(EditMessageAction::Replace {
        sequence,
        text: text.to_owned(),
    })
}

fn goal(_app: &mut App, mut context: Context<'_>) -> CommandResult<AppAction> {
    let text = context.args.next();
    match text {
        Some("pause") => {
            context.args.finish()?;
            Ok(AppAction::PauseGoal)
        }
        Some("resume") => {
            context.args.finish()?;
            Ok(AppAction::ResumeGoal)
        }
        Some("clear") => {
            context.args.finish()?;
            Ok(AppAction::ClearGoal)
        }
        Some(first) => {
            let mut instruction = first.to_owned();
            for part in context.args {
                instruction.push(' ');
                instruction.push_str(part);
            }
            Ok(AppAction::SetGoal { instruction })
        }
        None => Ok(AppAction::ShowGoal),
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
            CommandSpec::new("resume", resume)
                .usage("[session_id|latest]")
                .summary("Switch active session or pick a session to resume"),
        )
        .command(
            CommandSpec::new("provider", provider)
                .usage("[use] <profile>")
                .summary("List or switch active provider profile"),
        )
        .command(
            CommandSpec::new("model", model)
                .usage("<name> [reasoning] [tier]")
                .summary("Switch active model settings"),
        )
        .command(CommandSpec::new("retry", retry).summary("Retry the current user/tool turn"))
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
        .command(CommandSpec::new("goal", goal).usage("<task>").summary(
            "Set a persisted goal; the agent loop keeps going until the model calls goal complete",
        ))
        .command(
            CommandSpec::new("edit", edit_message)
                .usage("[<sequence> <text>|delete <sequence>]")
                .summary("Edit messages or delete messages and tool calls"),
        )
        .command(
            CommandSpec::new("rewind", rewind)
                .usage("[compact] [before:<sequence>]")
                .summary(
                    "Fork the current session, optionally rewinding before the last compaction",
                ),
        )
        .build()?)
}

async fn activate_session_fork(
    new_session_id: harness_session_store::SessionId,
    transport_session_id: &str,
    app_state: &mut App,
    provider_config: &ProviderConfig,
    runtime: &mut ConversationRuntime,
    event_tx: &harness_runtime_api::RuntimeEventSender,
    session_root: &Path,
    active_generation: &Arc<AtomicU64>,
    event_sequence: &mut u64,
) -> Result<(), String> {
    let active_profile_id =
        harness_provider::ProviderProfileId::new(app_state.active_profile.as_str());
    let (model_slug, reasoning_effort, service_tier) = {
        let selection = runtime.active_model();
        (
            selection.model.clone(),
            selection.reasoning_effort.clone(),
            selection.service_tier.clone(),
        )
    };
    let next_generation = active_generation.load(Ordering::Acquire) + 1;
    let (resolved, transport) = resolve_provider_and_transport(
        provider_config,
        transport_session_id,
        &active_profile_id,
        &model_slug,
        next_generation,
        reasoning_effort,
        service_tier,
    )
    .await
    .map_err(|error| format!("Failed to rebuild transport for fork: {error}"))?;

    runtime
        .switch_session(new_session_id.clone())
        .await
        .map_err(|error| format!("Failed to switch to fork: {error}"))?;
    runtime.update_ports(
        transport,
        resolved.routes.root.clone(),
        resolved.capabilities.freeform_tool_input,
    );
    runtime.update_compaction_route(resolved.routes.compaction.clone());
    active_generation.store(next_generation, Ordering::Release);
    app_state.session_id = new_session_id.as_str().to_owned();

    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
        *event_sequence,
        harness_runtime_api::RuntimeEvent::SessionChanged(new_session_id.as_str().to_owned()),
    );
    *event_sequence += 1;
    event_tx
        .send(envelope)
        .await
        .map_err(|_| "Event channel closed".to_string())?;
    emit_transcript_replacement(session_root, &new_session_id, event_tx, event_sequence).await
}
async fn execute_app_action(
    action: AppAction,
    app_state: &mut App,
    provider_config: &ProviderConfig,
    runtime: &mut ConversationRuntime,
    command_rx: &mut harness_runtime_api::RuntimeCommandReceiver,
    commands: &harness_runtime_api::RuntimeCommandSender,
    deferred_commands: &mut VecDeque<harness_runtime_api::RuntimeCommand>,
    suspended_effects: &mut Option<Vec<RuntimeEffect>>,

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

            runtime.update_ports(
                new_transport,
                resolved.routes.root.clone(),
                resolved.capabilities.freeform_tool_input,
            );
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
            if !drive_runtime_effects(
                runtime,
                command_rx,
                commands,
                deferred_commands,
                suspended_effects,
                effects,
                event_tx,
                seq,
            )
            .await
            {
                return Err("Runtime event channel closed".to_string());
            }
            Ok(())
        }
        AppAction::SetGoal { instruction } => {
            let goal_text = instruction.clone();
            runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::SetGoal {
                    instruction: instruction.clone(),
                })
                .await
                .map_err(|error| format!("Failed to set goal: {error}"))?;
            let _ = runtime
                .append_records(&[harness_session_store::SessionPayload::Goal {
                    instruction: instruction.clone(),
                    state: "active".to_string(),
                }])
                .await;
            // Submit the goal as a prompt to start the agent loop.
            let effects = runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::SubmitPrompt {
                    text: goal_text,
                })
                .await
                .map_err(|error| format!("Failed to submit goal prompt: {error}"))?;
            if !drive_runtime_effects(
                runtime,
                command_rx,
                commands,
                deferred_commands,
                suspended_effects,
                effects,
                event_tx,
                seq,
            )
            .await
            {
                return Err("Runtime event channel closed".to_string());
            }
            Ok(())
        }
        AppAction::PauseGoal => {
            let goal_instruction = match runtime.persist_state() {
                harness_conversation_runtime::PersistState::Active(task)
                | harness_conversation_runtime::PersistState::Paused(task) => {
                    task.instruction.clone()
                }
                _ => String::new(),
            };
            runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::PauseGoal)
                .await
                .map_err(|error| format!("Failed to pause goal: {error}"))?;
            let _ = runtime
                .append_records(&[harness_session_store::SessionPayload::Goal {
                    instruction: goal_instruction,
                    state: "paused".to_string(),
                }])
                .await;
            emit_text_message(text, "Goal paused.", runtime, event_tx, seq).await;
            Ok(())
        }
        AppAction::ResumeGoal => {
            let goal_instruction = match runtime.persist_state() {
                harness_conversation_runtime::PersistState::Active(task)
                | harness_conversation_runtime::PersistState::Paused(task) => {
                    task.instruction.clone()
                }
                _ => String::new(),
            };
            runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::ResumeGoal)
                .await
                .map_err(|error| format!("Failed to resume goal: {error}"))?;
            let _ = runtime
                .append_records(&[harness_session_store::SessionPayload::Goal {
                    instruction: goal_instruction,
                    state: "active".to_string(),
                }])
                .await;
            emit_text_message(text, "Goal resumed.", runtime, event_tx, seq).await;
            Ok(())
        }
        AppAction::ClearGoal => {
            let goal_instruction = match runtime.persist_state() {
                harness_conversation_runtime::PersistState::Active(task)
                | harness_conversation_runtime::PersistState::Paused(task) => {
                    task.instruction.clone()
                }
                _ => String::new(),
            };
            runtime
                .dispatch_command(harness_runtime_api::RuntimeCommand::ClearGoal)
                .await
                .map_err(|error| format!("Failed to clear goal: {error}"))?;
            let _ = runtime
                .append_records(&[harness_session_store::SessionPayload::Goal {
                    instruction: goal_instruction,
                    state: "cleared".to_string(),
                }])
                .await;
            emit_text_message(text, "Goal cleared.", runtime, event_tx, seq).await;
            Ok(())
        }
        AppAction::ShowGoal => {
            let msg = match runtime.persist_state() {
                harness_conversation_runtime::PersistState::Active(task) => {
                    format!("Active goal: {}", task.instruction)
                }
                harness_conversation_runtime::PersistState::Paused(task) => {
                    format!("Paused goal: {}", task.instruction)
                }
                harness_conversation_runtime::PersistState::Completed(task) => {
                    format!("Completed goal: {}", task.instruction)
                }
                _ => "No active goal set.".to_string(),
            };
            emit_text_message(text, &msg, runtime, event_tx, seq).await;
            Ok(())
        }
        AppAction::ResumeSession { session_id } => {
            if runtime.phase() != &harness_conversation_runtime::ConversationPhase::Idle {
                return Err(
                    "Sessions can be resumed only while no response or tool is active."
                        .to_string(),
                );
            }
            let target_id = match session_id.as_deref() {
                None | Some("pick") | Some("") => {
                    let sessions = picker::list_sessions(session_root)
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .map(|s| harness_runtime_api::SessionPickerMeta {
                            id: s.id,
                            modified_secs: s
                                .modified
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                            all_text: s.all_text,
                            model: s.model,
                            title: s.title,
                            initial_entries: s.initial_entries,
                        })
                        .collect();
                    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
                        *seq,
                        harness_runtime_api::RuntimeEvent::OpenSessionPicker(sessions),
                    );
                    *seq += 1;
                    if event_tx.send(envelope).await.is_err() {
                        return Err("Event channel closed".to_string());
                    }
                    return Ok(());
                }
                Some("latest") => latest_session_id(session_root).map_err(|e| e.to_string())?,
                Some(raw) => resolve_session_id(raw.to_string()).map_err(|e| e.to_string())?,
            };
            if let Err(e) = runtime.switch_session(target_id.clone()).await {
                return Err(format!("Failed to switch session: {e}"));
            }
            app_state.session_id = target_id.as_str().to_string();
            let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
                *seq,
                harness_runtime_api::RuntimeEvent::SessionChanged(target_id.as_str().to_string()),
            );
            *seq += 1;
            if event_tx.send(envelope).await.is_err() {
                return Err("Event channel closed".to_string());
            }
            emit_transcript_replacement(session_root, &target_id, event_tx, seq).await?;
            let msg = format!("Switched to session {}", target_id.as_str());
            emit_text_message("", &msg, runtime, event_tx, seq).await;
            Ok(())
        }
        AppAction::EditMessage(action) => {
            if runtime.phase() != &harness_conversation_runtime::ConversationPhase::Idle {
                return Err(
                    "Transcript messages can be edited only while no response or tool is active."
                        .to_string(),
                );
            }
            let current_id = harness_session_store::SessionId::new(app_state.session_id.clone())
                .map_err(|error| format!("Invalid current session id: {error}"))?;
            let records = read_session_records(session_root, &current_id)
                .map_err(|error| format!("Failed to read current session: {error}"))?;

            match action {
                EditMessageAction::Choose => {
                    let messages = records
                        .iter()
                        .filter_map(editable_message)
                        .collect::<Vec<_>>();
                    if messages.is_empty() {
                        return Err(
                            "The active session has no persisted user or assistant messages to \
                             edit."
                                .to_string(),
                        );
                    }
                    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
                        *seq,
                        harness_runtime_api::RuntimeEvent::OpenMessageEditor(messages),
                    );
                    *seq += 1;
                    event_tx
                        .send(envelope)
                        .await
                        .map_err(|_| "Event channel closed".to_string())
                }
                EditMessageAction::Replace { sequence, text } => {
                    let payloads = edit_session_records(&records, sequence, Some(text.as_str()))?;
                    let new_id = create_session_fork(session_root, &payloads)
                        .map_err(|error| format!("Failed to create edited session: {error}"))?;
                    let transport_session_id = new_id.as_str().to_owned();
                    activate_session_fork(
                        new_id.clone(),
                        &transport_session_id,
                        app_state,
                        provider_config,
                        runtime,
                        event_tx,
                        session_root,
                        active_generation,
                        seq,
                    )
                    .await?;
                    emit_notification(
                        &format!(
                            "Edited message #{sequence}. Active fork: {}",
                            new_id.as_str()
                        ),
                        event_tx,
                        seq,
                    )
                    .await;
                    Ok(())
                }
                EditMessageAction::Delete { sequence } => {
                    let payloads = edit_session_records(&records, sequence, None)?;
                    let new_id = create_session_fork(session_root, &payloads)
                        .map_err(|error| format!("Failed to create edited session: {error}"))?;
                    let transport_session_id = new_id.as_str().to_owned();
                    activate_session_fork(
                        new_id.clone(),
                        &transport_session_id,
                        app_state,
                        provider_config,
                        runtime,
                        event_tx,
                        session_root,
                        active_generation,
                        seq,
                    )
                    .await?;
                    emit_notification(
                        &format!(
                            "Deleted message #{sequence}. Active fork: {}",
                            new_id.as_str()
                        ),
                        event_tx,
                        seq,
                    )
                    .await;
                    Ok(())
                }
            }
        }
        AppAction::Rewind {
            before_compaction,
            before_sequence,
        } => {
            if runtime.phase() != &harness_conversation_runtime::ConversationPhase::Idle {
                return Err(
                    "A session can be rewound only while no response or tool is active."
                        .to_string(),
                );
            }
            // Read all records from the current session.
            let current_session_id_str = app_state.session_id.clone();
            let current_id = harness_session_store::SessionId::new(current_session_id_str.clone())
                .map_err(|e| format!("Invalid current session id: {e}"))?;
            let records = read_session_records(session_root, &current_id)
                .map_err(|e| format!("Failed to read current session: {e}"))?;

            // Determine the cutpoint.
            let records_to_keep: Vec<&SessionRecord> = if before_compaction {
                // Find the last CompactionCheckpoint and keep only records before it.
                let last_compaction_pos = records.iter().rposition(|r| {
                    matches!(r.payload, SessionPayload::CompactionCheckpoint { .. })
                });
                match last_compaction_pos {
                    Some(pos) => records[..pos].iter().collect(),
                    None => {
                        return Err("No compaction checkpoint found in current session".to_string());
                    }
                }
            } else if let Some(cutseq) = before_sequence {
                // Keep records whose sequence is strictly less than cutseq.
                records.iter().filter(|r| r.sequence < cutseq).collect()
            } else {
                let mut turn_options = Vec::new();
                for record in &records {
                    match &record.payload {
                        SessionPayload::InputMessage { text, .. } => {
                            let snippet: String = text.chars().take(60).collect();
                            turn_options.push(harness_runtime_api::RewindOptionMeta {
                                sequence: record.sequence,
                                label: format!("[#{}] User: {}", record.sequence, snippet),
                            });
                        }
                        SessionPayload::CompactionCheckpoint { summary, .. } => {
                            let snippet: String = summary.chars().take(60).collect();
                            turn_options.push(harness_runtime_api::RewindOptionMeta {
                                sequence: record.sequence,
                                label: format!(
                                    "[#{}] ── Compaction: {} ──",
                                    record.sequence, snippet
                                ),
                            });
                        }
                        _ => {}
                    }
                }
                if turn_options.is_empty() {
                    return Err("No turn or checkpoint available to rewind".to_string());
                }
                let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
                    *seq,
                    harness_runtime_api::RuntimeEvent::OpenRewindPicker(turn_options),
                );
                *seq += 1;
                if event_tx.send(envelope).await.is_err() {
                    return Err("Event channel closed".to_string());
                }
                return Ok(());
            };

            let rewind_sequence = records_to_keep
                .last()
                .map(|record| record.sequence)
                .unwrap_or(0);
            let payloads = records_to_keep
                .into_iter()
                .map(|record| record.payload.clone())
                .collect::<Vec<_>>();
            let new_id = create_session_fork(session_root, &payloads)
                .map_err(|error| format!("Failed to create rewind fork: {error}"))?;
            activate_session_fork(
                new_id.clone(),
                &current_session_id_str,
                app_state,
                provider_config,
                runtime,
                event_tx,
                session_root,
                active_generation,
                seq,
            )
            .await?;

            emit_notification(
                &format!(
                    "Rewound session before sequence {rewind_sequence}. Active fork: {}",
                    new_id.as_str()
                ),
                event_tx,
                seq,
            )
            .await;
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
            if !drive_runtime_effects(
                runtime,
                command_rx,
                commands,
                deferred_commands,
                suspended_effects,
                effects,
                event_tx,
                seq,
            )
            .await
            {
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
            if !drive_runtime_effects(
                runtime,
                command_rx,
                commands,
                deferred_commands,
                suspended_effects,
                effects,
                event_tx,
                seq,
            )
            .await
            {
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

            runtime.update_ports(
                new_transport,
                resolved.routes.root.clone(),
                resolved.capabilities.freeform_tool_input,
            );
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
            emit_notification(&outcome_text, event_tx, seq).await;

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
    deferred_commands: &mut VecDeque<harness_runtime_api::RuntimeCommand>,
    turn_id: u64,
    attempt: Arc<ModelAttempt>,
    route: ResolvedModelRoute,
    event_tx: &harness_runtime_api::RuntimeEventSender,
    seq: &mut u64,
) -> Result<Option<Vec<RuntimeEffect>>, RuntimeError> {
    let attempt_id = attempt.attempt_id.0;
    let used_previous_response = attempt.request.previous_response_id.is_some();
    let compaction_attempt = runtime.compaction_attempt_active();
    ConversationRuntime::build_active_attempt(turn_id, Arc::clone(&attempt), route)?;

    let active_transport = runtime.active_transport();
    let mut handle = match active_transport.start(attempt).await {
        Ok(handle) => handle,
        Err(error) => {
            if compaction_attempt {
                let effects = runtime
                    .fail_compaction_attempt(
                        harness_runtime_api::RuntimeFailureCategory::Model,
                        error.message.clone(),
                    )
                    .await?;
                return Ok(Some(effects));
            }

            let outcome = harness_model_api::ModelTerminalOutcome::Failed(error);
            if used_previous_response && requires_full_context_retry(&outcome) {
                if let Some(retry) = runtime.retry_with_full_context(turn_id, attempt_id)? {
                    return Ok(Some(vec![
                        RuntimeEffect::Emit(harness_runtime_api::RuntimeEvent::Failure(
                            harness_runtime_api::RuntimeFailure {
                                category: harness_runtime_api::RuntimeFailureCategory::Model,
                                message: "The provider rejected the cached response continuation; retrying with full context.".to_string(),
                            },
                        )),
                        retry,
                    ]));
                }
            }

            if is_custom_tool_compatibility_failure(&outcome) {
                if let Some(retry) = runtime.retry_with_compatibility(turn_id, attempt_id)? {
                    return Ok(Some(vec![
                        RuntimeEffect::Emit(
                            harness_runtime_api::RuntimeEvent::Failure(
                                harness_runtime_api::RuntimeFailure {
                                    category: harness_runtime_api::RuntimeFailureCategory::Model,
                                    message: "Failed to connect to model".to_string(),
                                }
                            )
                        ),
                        RuntimeEffect::Emit(harness_runtime_api::RuntimeEvent::Failure(
                            harness_runtime_api::RuntimeFailure {
                                category: harness_runtime_api::RuntimeFailureCategory::Model,
                                message: "The provider rejected native custom-tool history; compatibility mode will be used and the request will be retried.".to_string(),
                            }
                        )),
                        retry,
                    ]));
                }
            }

            let effects = runtime
                .finish_model_attempt(turn_id, attempt_id, outcome)
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
            biased;
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
                if matches!(
                    &command,
                    harness_runtime_api::RuntimeCommand::ExecuteCommand { .. }
                ) {
                    deferred_commands.push_back(command);
                    continue;
                }

                let is_interrupt = matches!(
                    command,
                    harness_runtime_api::RuntimeCommand::Interrupt { .. }
                        | harness_runtime_api::RuntimeCommand::SendQueuedSteering
                        | harness_runtime_api::RuntimeCommand::CancelCompaction
                        | harness_runtime_api::RuntimeCommand::StopRequestLoop
                        | harness_runtime_api::RuntimeCommand::AbortResponse
                );
                if matches!(
                    &command,
                    harness_runtime_api::RuntimeCommand::Compact { .. }
                ) {
                    // Compaction requires an idle runtime. Preserve the command for
                    // the outer loop without cancelling the active response.
                    let _ = commands.try_send(command);
                    continue;
                }
                if !is_interrupt {
                    // QueueSteering: queue text for the next attempt and let the
                    // current stream finish naturally — do NOT cancel.
                    if matches!(
                        &command,
                        harness_runtime_api::RuntimeCommand::QueueSteering { .. }
                            | harness_runtime_api::RuntimeCommand::SubmitPrompt { .. }
                    ) {
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
                let may_cancel = !matches!(
                    command,
                    harness_runtime_api::RuntimeCommand::StopRequestLoop
                );
                match runtime.dispatch_command(command).await {
                    Ok(effects) => {
                        for effect in effects {
                            match effect {
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
                    Err(error) => {
                        emit_runtime_failure(
                            event_tx,
                            seq,
                            harness_runtime_api::RuntimeFailureCategory::Command,
                            format!("runtime command failed: {error}"),
                        )
                        .await;
                    }
                }
                if may_cancel && runtime.model_cancellation_requested() {
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
        let full_context_failure = match &event {
            harness_model_api::ModelEvent::Terminal(outcome)
                if used_previous_response
                    && !compaction_attempt
                    && requires_full_context_retry(outcome) =>
            {
                Some(outcome.clone())
            }
            _ => None,
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
        let (event_effects, stop) = if let Some(outcome) = full_context_failure {
            let started = runtime.model_response_started();
            if let Some(retry) = runtime.retry_with_full_context(turn_id, attempt_id)? {
                let mut effects = Vec::new();
                if started {
                    effects.push(RuntimeEffect::Emit(
                        harness_runtime_api::RuntimeEvent::ResponseFinished(outcome),
                    ));
                }
                effects.push(RuntimeEffect::Emit(harness_runtime_api::RuntimeEvent::Failure(
                    harness_runtime_api::RuntimeFailure {
                        category: harness_runtime_api::RuntimeFailureCategory::Model,
                        message: "The provider rejected the cached response continuation; retrying with full context.".to_string(),
                    },
                )));
                effects.push(retry);
                (effects, true)
            } else {
                (
                    runtime
                        .finish_model_attempt(turn_id, attempt_id, outcome)
                        .await?,
                    true,
                )
            }
        } else if let Some(outcome) = compatibility_failure {
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
                (
                    runtime
                        .finish_model_attempt(turn_id, attempt_id, outcome)
                        .await?,
                    true,
                )
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
                        let message = format!("model event rejected: {error}");
                        (
                            runtime
                                .fail_compaction_attempt(
                                    harness_runtime_api::RuntimeFailureCategory::Protocol,
                                    message,
                                )
                                .await?,
                            true,
                        )
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
        let effects = runtime
            .fail_compaction_attempt(
                harness_runtime_api::RuntimeFailureCategory::Protocol,
                "model stream ended before a terminal event".to_string(),
            )
            .await?;
        return Ok(Some(effects));
    }
    let effects = runtime
        .finish_model_attempt(turn_id, attempt_id, failure)
        .await?;
    deferred.extend(effects);
    Ok(Some(deferred))
}

fn is_custom_tool_compatibility_failure(outcome: &harness_model_api::ModelTerminalOutcome) -> bool {
    matches!(
        outcome,
        harness_model_api::ModelTerminalOutcome::Failed(failure)
            if (failure.kind == harness_model_api::ModelFailureKind::ProviderRejected
                || failure.kind == harness_model_api::ModelFailureKind::Transport
                || failure.kind == harness_model_api::ModelFailureKind::Protocol)
                && (failure.message.contains("HTTP status 400") || failure.message.contains("invalid_request_error"))
                && failure.message.contains("custom_tool")
    )
}

fn requires_full_context_retry(outcome: &harness_model_api::ModelTerminalOutcome) -> bool {
    matches!(
        outcome,
        harness_model_api::ModelTerminalOutcome::Failed(failure)
            if failure.message.contains("previous_response_not_found")
                || failure.message.contains("previous_response_id")
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
    deferred_commands: &mut VecDeque<harness_runtime_api::RuntimeCommand>,
    suspended_effects: &mut Option<Vec<RuntimeEffect>>,
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
                if let Some(usage) = attempt.request.context_usage
                    && !emit_runtime_event(
                        event_tx,
                        seq,
                        harness_runtime_api::RuntimeEvent::ContextUsage(
                            harness_runtime_api::ContextUsage {
                                estimated_input_tokens: usage.estimated_input_tokens,
                                max_input_tokens: usage.max_input_tokens,
                                compact_at_tokens: usage.compact_at_tokens,
                            },
                        ),
                    )
                    .await
                {
                    return false;
                }
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
                let result = run_model_attempt(
                    runtime,
                    command_rx,
                    commands,
                    deferred_commands,
                    turn_id,
                    attempt,
                    route,
                    event_tx,
                    seq,
                )
                .await;
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
            }

            RuntimeEffect::StartCompaction {
                compaction_id: _,
                attempt,
                route,
            } => match run_model_attempt(
                runtime,
                command_rx,
                commands,
                deferred_commands,
                0,
                attempt,
                route,
                event_tx,
                seq,
            )
            .await
            {
                Ok(Some(effects)) => Ok(effects),
                Ok(None) => return false,
                Err(error) => {
                    let message = format!("starting compaction failed: {error}");
                    runtime
                        .fail_compaction_attempt(
                            harness_runtime_api::RuntimeFailureCategory::Model,
                            message.clone(),
                        )
                        .await
                        .map_err(|cleanup_error| {
                            (
                                harness_runtime_api::RuntimeFailureCategory::Lifecycle,
                                format!("{message}; compaction cleanup failed: {cleanup_error}"),
                            )
                        })
                }
            },

            RuntimeEffect::CommitCompaction { compaction_id } => {
                match runtime.commit_compaction(compaction_id).await {
                    Ok(effects) => Ok(effects),
                    Err(error) => {
                        let message = format!("compaction commit failed: {error}");
                        Ok(vec![
                            RuntimeEffect::Emit(
                                harness_runtime_api::RuntimeEvent::CompactionFailed(
                                    message.clone(),
                                ),
                            ),
                            RuntimeEffect::Emit(
                                harness_runtime_api::RuntimeEvent::AgenticLoopCompleted,
                            ),
                            RuntimeEffect::Emit(harness_runtime_api::RuntimeEvent::Failure(
                                harness_runtime_api::RuntimeFailure {
                                    category:
                                        harness_runtime_api::RuntimeFailureCategory::Session,
                                    message,
                                },
                            )),
                        ])
                    }
                }
            }
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
            RuntimeEffect::ContinueModel { turn_id: _ } => {
                runtime.start_model_request().await.map_err(|error| {
                    (
                        harness_runtime_api::RuntimeFailureCategory::Model,
                        format!("model continuation failed: {error}"),
                    )
                })
            }
            RuntimeEffect::ExecuteTool {
                turn_id,
                call_id,
                request,
            } => {
                let execution_id = request.execution_id.0;
                let result = match runtime.tool_executor() {
                    Ok(executor) => executor.execute(request).await,
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
                    .commit_tool_result(turn_id, execution_id, call_id, result)
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
            Ok(next) => {
                enqueue_runtime_effects(&mut pending, next);
                if !deferred_commands.is_empty() {
                    let mut suspended = pending.into_iter().rev().collect::<Vec<_>>();
                    if let Some(existing) = suspended_effects.take() {
                        suspended.extend(existing);
                    }
                    *suspended_effects = Some(suspended);
                    return true;
                }
            }
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
    freeform_tool_input: bool,
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
            freeform_tool_input,
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
        let mut deferred_commands = VecDeque::new();
        let mut suspended_effects = None;

        loop {
            let cmd = if let Some(command) = deferred_commands.pop_front() {
                command
            } else if let Some(effects) = suspended_effects.take() {
                if !drive_runtime_effects(
                    &mut runtime,
                    &mut command_rx,
                    &commands,
                    &mut deferred_commands,
                    &mut suspended_effects,
                    effects,
                    &event_tx,
                    &mut seq,
                )
                .await
                {
                    break;
                }
                continue;
            } else {
                let Ok(command) = command_rx.recv().await else {
                    break;
                };
                command
            };
            let is_shutdown = matches!(&cmd, harness_runtime_api::RuntimeCommand::Shutdown);

            if let harness_runtime_api::RuntimeCommand::ExecuteCommand { text } = &cmd {
                let result = match registry.dispatch(&mut app_state, text) {
                    Ok(Dispatch::Ran(action)) => {
                        execute_app_action(
                            action,
                            &mut app_state,
                            &provider_config,
                            &mut runtime,
                            &mut command_rx,
                            &commands,
                            &mut deferred_commands,
                            &mut suspended_effects,
                            &event_tx,
                            &session_root,
                            &active_generation,
                            &mut seq,
                            text,
                        )
                        .await
                    }
                    Ok(Dispatch::NotCommand) => Err(
                        "failed to execute command: command text must begin with `/`; submit ordinary text as a prompt"
                            .to_string(),
                    ),
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

            if !drive_runtime_effects(
                &mut runtime,
                &mut command_rx,
                &commands,
                &mut deferred_commands,
                &mut suspended_effects,
                effects,
                &event_tx,
                &mut seq,
            )
            .await
            {
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

    RuntimeHandle {
        commands: commands_handle,
        events,
    }
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
                (
                    default_id,
                    default_profile.default_model.clone(),
                    None,
                    None,
                )
            }
        } else {
            let default_id = provider_config.default_profile_id.clone();
            let default_profile = provider_config.profile(&default_id).ok_or_else(|| {
                CliError::ProviderRuntime(ProviderError::InvalidProfile(format!(
                    "default provider profile not found: {default_id}"
                )))
            })?;
            (
                default_id,
                default_profile.default_model.clone(),
                None,
                None,
            )
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
            provider_config: provider_config.clone(),
            base_instructions: load_base_instructions()?,
            store: resolved_provider.capabilities.store,
        }),
        model_route: resolved_provider.routes.root.clone(),
        compaction_route: resolved_provider.routes.compaction.clone(),
        tool_availability,
    };

    let runtime = start_conversation_runtime(
        session_id,
        resolved_provider.selected_model.clone(),
        resolved_provider.capabilities.freeform_tool_input,
        ports,
        provider_config,
        active_generation,
        session_root,
    );

    let final_state =
        harness_tui_rewrite::run_with_runtime(snapshot, runtime.commands, runtime.events).await?;

    let mut stdout = io::stdout();
    writeln!(
        stdout,
        "Resume conversation ID: {}",
        final_state.session_id.as_str()
    )?;
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
    let initial_transcript_entries =
        harness_conversation_runtime::project_transcript_records(&records);

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

fn editable_message(record: &SessionRecord) -> Option<harness_runtime_api::EditableMessage> {
    let (role, text) = match &record.payload {
        SessionPayload::InputMessage { text, .. } => {
            (harness_runtime_api::EditableMessageRole::User, text.clone())
        }
        SessionPayload::AssistantMessage { text, .. } => (
            harness_runtime_api::EditableMessageRole::Assistant,
            text.clone(),
        ),
        _ => return None,
    };
    Some(harness_runtime_api::EditableMessage {
        sequence: record.sequence,
        role,
        text,
    })
}

fn edit_session_records(
    records: &[SessionRecord],
    target_sequence: u64,
    replacement: Option<&str>,
) -> Result<Vec<SessionPayload>, String> {
    let target = records
        .iter()
        .find(|record| record.sequence == target_sequence)
        .ok_or_else(|| {
            format!(
                "No persisted transcript record has sequence {target_sequence}. \
                 Run `/edit` to choose a transcript entry."
            )
        })?;
    let tool_target = match &target.payload {
        SessionPayload::InputMessage { .. } | SessionPayload::AssistantMessage { .. } => None,
        SessionPayload::ToolCallAccepted {
            turn_id, call_id, ..
        } if replacement.is_none() => Some((*turn_id, call_id.as_str())),
        SessionPayload::ToolCallAccepted { .. } => {
            return Err(format!(
                "Transcript record {target_sequence} is a tool activity. Typed tool invocations \
                 cannot be edited as raw text. Use `/edit delete {target_sequence}` to remove the \
                 call and its result."
            ));
        }
        _ => {
            return Err(format!(
                "Transcript record {target_sequence} is not an editable user or assistant \
                 message. Run `/edit` to choose a transcript entry."
            ));
        }
    };

    let mut payloads = Vec::with_capacity(records.len());
    for record in records {
        if record.sequence == target_sequence {
            if let Some(text) = replacement {
                let payload = match &record.payload {
                    SessionPayload::InputMessage { turn_id, .. } => SessionPayload::InputMessage {
                        turn_id: *turn_id,
                        text: text.to_owned(),
                    },
                    SessionPayload::AssistantMessage { turn_id, .. } => {
                        SessionPayload::AssistantMessage {
                            turn_id: *turn_id,
                            text: text.to_owned(),
                        }
                    }
                    _ => unreachable!("target payload was validated as an editable message"),
                };
                payloads.push(payload);
            }
            continue;
        }

        let correlated_tool_record = tool_target.is_some_and(|(target_turn_id, target_call_id)| {
            matches!(
                &record.payload,
                SessionPayload::ToolExecutionFinished {
                    turn_id, call_id, ..
                }
                    if *turn_id == target_turn_id && call_id == target_call_id
            )
        });
        if correlated_tool_record {
            continue;
        }

        let stale_derived_state = record.sequence > target_sequence
            && matches!(
                record.payload,
                SessionPayload::CompactionCheckpoint { .. }
                    | SessionPayload::ModelResponseMetadata { .. }
            );
        if !stale_derived_state {
            payloads.push(record.payload.clone());
        }
    }
    Ok(payloads)
}

fn create_session_fork(
    root: &Path,
    payloads: &[SessionPayload],
) -> CliResult<harness_session_store::SessionId> {
    let session_id = harness_session_store::SessionId::new(generate_session_id()?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let session_dir = root.join("sessions");
    std::fs::create_dir_all(&session_dir)?;
    let path = session_dir.join(format!("{}.json", session_id.as_str()));
    let records = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            let sequence = u64::try_from(index)
                .ok()
                .and_then(|index| index.checked_add(1))
                .ok_or_else(|| {
                    CliError::Session(SessionStoreError::InvalidFormat(
                        "session sequence space is exhausted".to_string(),
                    ))
                })?;
            Ok(SerializableRecord {
                sequence,
                payload: to_serializable_payload(payload),
            })
        })
        .collect::<CliResult<Vec<_>>>()?;
    let json = serde_json::to_string_pretty(&records)
        .map_err(|error| CliError::Session(SessionStoreError::InvalidFormat(error.to_string())))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    Ok(session_id)
}

async fn emit_transcript_replacement(
    root: &Path,
    session_id: &harness_session_store::SessionId,
    event_tx: &harness_runtime_api::RuntimeEventSender,
    sequence: &mut u64,
) -> Result<(), String> {
    let records = read_session_records(root, session_id)
        .map_err(|error| format!("Failed to read forked session: {error}"))?;
    let entries = harness_conversation_runtime::project_transcript_records(&records);
    let envelope = harness_runtime_api::RuntimeEventEnvelope::new(
        *sequence,
        harness_runtime_api::RuntimeEvent::TranscriptReplaced(entries),
    );
    *sequence += 1;
    event_tx
        .send(envelope)
        .await
        .map_err(|_| "Event channel closed".to_string())
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
    let loaded = deserialize_session_records(&content).map_err(CliError::Session)?;
    Ok(loaded
        .into_iter()
        .map(|record| from_serializable_record(record, session_id))
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    use futures_util::StreamExt;
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;

    #[test]
    fn legacy_tool_records_are_upgraded_when_loading_a_session() {
        let content = serde_json::json!([
            {
                "sequence": 1,
                "payload": { "ToolCallAccepted": {
                    "turn_id": 7,
                    "call_id": "call-1",
                    "name": "inspect",
                    "input": { "Freeform": "pwd" }
                }}
            },
            {
                "sequence": 2,
                "payload": { "ToolExecutionStarted": {
                    "turn_id": 7,
                    "call_id": "call-1"
                }}
            },
            {
                "sequence": 3,
                "payload": { "ToolExecutionFinished": {
                    "turn_id": 7,
                    "call_id": "call-1",
                    "output": "/workspace"
                }}
            }
        ]).to_string();

        let records = deserialize_session_records(&content).unwrap();
        assert_eq!(records.len(), 2);
        let session_id = harness_session_store::SessionId::new("legacy").unwrap();
        let accepted = from_serializable_record(records[0].clone(), &session_id);
        let finished = from_serializable_record(records[1].clone(), &session_id);

        assert!(matches!(accepted.payload, SessionPayload::ToolCallAccepted {
            raw_input: SessionToolInput::Freeform(ref input), ..
        } if input == "pwd"));
        assert!(matches!(finished.payload, SessionPayload::ToolExecutionFinished {
            raw_output: ref output,
            outcome: harness_tool_api::ToolOutcome::Failed(_),
            ..
        } if output == "/workspace"));
    }

    async fn retrying_http_server(
        status: http::StatusCode,
        failures_before_success: usize,
    ) -> (String, tokio::task::JoinHandle<usize>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut request_count = 0;
            while request_count <= failures_before_success {
                let (stream, _) = listener.accept().await.unwrap();
                request_count += 1;
                let request_number = request_count;
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |_request| async move {
                            let response = if request_number <= failures_before_success {
                                http::Response::builder()
                                    .status(status)
                                    .header(http::header::CONNECTION, "close")
                                    .body(Full::new(bytes::Bytes::from_static(b"retry")))
                                    .unwrap()
                            } else {
                                http::Response::builder()
                                    .status(http::StatusCode::OK)
                                    .header(http::header::CONTENT_TYPE, "text/event-stream")
                                    .header(http::header::CONNECTION, "close")
                                    .body(Full::new(bytes::Bytes::from_static(b"data: {}\n\n")))
                                    .unwrap()
                            };
                            Ok::<_, Infallible>(response)
                        }),
                    )
                    .await
                    .unwrap();
            }
            request_count
        });
        (format!("http://{addr}"), server)
    }

    fn http_responses_attempt() -> Arc<ModelAttempt> {
        let selection = ModelSelection::new(
            harness_model_api::ProviderId::new("test-provider").unwrap(),
            "test-model",
            None,
            None,
        );
        Arc::new(ModelAttempt::initial(
            Arc::new(ModelRequest {
                request_id: ModelRequestId(1),
                context_usage: None,
                provider_generation: ProviderGeneration(1),
                history_revision: 0,
                selection,
                input: Arc::from([]),
                tools: Arc::from([]),
                previous_response_id: None,
            }),
            harness_model_api::ModelAttemptId(1),
        ))
    }

    #[tokio::test]
    async fn http_provider_clients_continue_retrying_429_and_500() {
        const FAILURES_BEFORE_SUCCESS: usize = 3;

        for status in [
            http::StatusCode::TOO_MANY_REQUESTS,
            http::StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let (base_url, server) =
                retrying_http_server(status, FAILURES_BEFORE_SUCCESS).await;
            let client = HttpClient::new(
                base_url,
                None,
                "test-session",
                harness_model_api::ModelCapabilities::default(),
            );
            let mut stream = ChatStreamingClient::start(
                &client,
                sonic_rs::json!({"model": "test-model", "stream": true}),
            )
            .await
            .unwrap();
            assert!(stream.next().await.is_some());
            assert_eq!(
                server.await.unwrap(),
                FAILURES_BEFORE_SUCCESS + 1,
                "chat completions status {status}"
            );

            let (base_url, server) =
                retrying_http_server(status, FAILURES_BEFORE_SUCCESS).await;
            let client = HttpClient::new(
                base_url,
                None,
                "test-session",
                harness_model_api::ModelCapabilities::default(),
            );
            let mut stream = StreamingClient::start(&client, http_responses_attempt())
                .await
                .unwrap();
            assert!(stream.next().await.is_some());
            assert_eq!(
                server.await.unwrap(),
                FAILURES_BEFORE_SUCCESS + 1,
                "Responses status {status}"
            );

        }
    }

    fn record(sequence: u64, payload: SessionPayload) -> SessionRecord {
        SessionRecord {
            session_id: harness_session_store::SessionId::new("test-session").unwrap(),
            sequence,
            payload,
        }
    }
    fn inspect_invocation(path: &str) -> harness_tool_api::ToolInvocation {
        harness_tool_api::ToolInvocation::Inspect(harness_tool_api::Prepared::Ready(
            harness_tool_api::InspectRequest {
                jobs: vec![harness_tool_api::InspectJobRequest::Read(
                    harness_tool_api::InspectReadRequest {
                        path: path.to_owned(),
                        ranges: vec![harness_tool_api::LineRange {
                            start_line: 1,
                            line_count: 20,
                        }],
                    },
                )],
            },
        ))
    }

    fn inspect_outcome(path: &str, lines: &[&str]) -> harness_tool_api::ToolOutcome {
        harness_tool_api::ToolOutcome::Inspect(harness_tool_api::InspectResult {
            jobs: vec![harness_tool_api::InspectJobOutcome::Succeeded(
                harness_tool_api::InspectJobSuccess::Read(harness_tool_api::InspectReadResult {
                    excerpts: vec![harness_tool_api::SourceExcerpt {
                        path: path.to_owned(),
                        start_line: 1,
                        lines: lines.iter().map(|line| (*line).to_owned()).collect(),
                        next: None,
                    }],
                }),
            )],
        })
    }

    struct CommandGateRequestBuilder;

    impl ModelRequestBuilder for CommandGateRequestBuilder {
        fn build(
            &self,
            revision: u64,
            selection: &ModelSelection,
            provider_generation: ProviderGeneration,
            history: &[SessionPayload],
            steering: &[String],
        ) -> Result<Arc<ModelRequest>, RuntimeError> {
            let mut input = history
                .iter()
                .filter_map(|payload| match payload {
                    SessionPayload::InputMessage { text, .. } => Some(ModelInput::Message {
                        role: ModelMessageRole::User,
                        text: text.clone(),
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>();
            input.extend(steering.iter().map(|text| ModelInput::Message {
                role: ModelMessageRole::User,
                text: text.clone(),
            }));
            Ok(Arc::new(ModelRequest {
                request_id: ModelRequestId(revision),
                context_usage: None,
                provider_generation,
                history_revision: revision,
                selection: selection.clone(),
                input: input.into(),
                tools: Arc::from([]),
                previous_response_id: None,
            }))
        }
    }

    struct ControlledAttemptHandle {
        release: Option<tokio::sync::oneshot::Receiver<()>>,
        events: tokio::sync::mpsc::UnboundedReceiver<harness_model_api::ModelEvent>,
        cancellations: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ModelAttemptHandle for ControlledAttemptHandle {
        fn cancel(&mut self, _reason: harness_model_api::ModelCancellation) {
            self.cancellations.fetch_add(1, Ordering::Relaxed);
        }
        fn next_event(
            &mut self,
        ) -> Pin<Box<dyn Future<Output = Option<harness_model_api::ModelEvent>> + Send + '_>>
        {
            Box::pin(async move {
                if let Some(release) = self.release.as_mut() {
                    let _ = release.await;
                    self.release = None;
                }
                self.events.recv().await
            })
        }
    }

    struct ControlledTransport {
        handle: std::sync::Mutex<Option<ControlledAttemptHandle>>,
    }

    impl ModelTransport for ControlledTransport {
        fn start(
            &self,
            _attempt: Arc<ModelAttempt>,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Box<dyn ModelAttemptHandle>, ModelFailure>> + Send + '_,
            >,
        > {
            Box::pin(async move {
                self.handle
                    .lock()
                    .unwrap()
                    .take()
                    .map(|handle| Box::new(handle) as Box<dyn ModelAttemptHandle>)
                    .ok_or_else(|| ModelFailure {
                        kind: harness_model_api::ModelFailureKind::Protocol,
                        message: "controlled transport handle already taken".to_string(),
                    })
            })
        }

        fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), ModelFailure>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn slash_command_received_during_a_model_attempt_never_becomes_steering() {
        let root = std::env::temp_dir().join(format!(
            "new-harness-command-gate-test-{}",
            generate_session_id().unwrap()
        ));
        let session_id = harness_session_store::SessionId::new("command-gate").unwrap();
        let provider = harness_model_api::ProviderId::new("test-provider").unwrap();
        let selection = ModelSelection::new(provider.clone(), "test-model", None, None);
        let route = ResolvedModelRoute::new(
            provider,
            ProviderGeneration(1),
            "test-route",
            selection.clone(),
        )
        .unwrap();
        let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (model_event_tx, model_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let transport: Arc<dyn ModelTransport> = Arc::new(ControlledTransport {
            handle: std::sync::Mutex::new(Some(ControlledAttemptHandle {
                release: Some(release_rx),
                events: model_event_rx,
                cancellations: Arc::clone(&cancellations),
            })),
        });
        let mut runtime = ConversationRuntime::new(RuntimeConfiguration {
            session_id,
            model: selection,
            freeform_tool_input: true,
            ports: RuntimePorts {
                session_store: Arc::new(FileSessionStore { root: root.clone() }),
                tool_registry: ToolRegistry::new(),
                tool_executor: None,
                model_transport: transport,
                request_builder: Arc::new(CommandGateRequestBuilder),
                model_route: route.clone(),
                compaction_route: route,
                tool_availability: Arc::new(RwLock::new(ToolAvailability::new())),
            },
        });
        runtime.begin_startup().await.unwrap();
        runtime.mark_ready().unwrap();
        let effects = runtime
            .dispatch_command(harness_runtime_api::RuntimeCommand::SubmitPrompt {
                text: "initial prompt".to_string(),
            })
            .await
            .unwrap();
        let (commands, mut runtime_events, event_tx, mut command_rx) = channel_pair(64);
        let command_submitter = commands.clone();
        let mut deferred_commands = VecDeque::new();
        let mut suspended_effects = None;
        let mut event_sequence = 1;

        let drive = drive_runtime_effects(
            &mut runtime,
            &mut command_rx,
            &commands,
            &mut deferred_commands,
            &mut suspended_effects,
            effects,
            &event_tx,
            &mut event_sequence,
        );
        let feed = async move {
            command_submitter
                .send(harness_runtime_api::RuntimeCommand::SendQueuedSteering)
                .await
                .unwrap();
            tokio::task::yield_now().await;
            command_submitter
                .send(harness_runtime_api::RuntimeCommand::ExecuteCommand {
                    text: "/model next-model high".to_string(),
                })
                .await
                .unwrap();
            tokio::task::yield_now().await;
            release_tx.send(()).unwrap();
            model_event_tx
                .send(harness_model_api::ModelEvent::Started)
                .unwrap();
            model_event_tx
                .send(harness_model_api::ModelEvent::AssistantTextDelta(
                    "done".to_string(),
                ))
                .unwrap();
            model_event_tx
                .send(harness_model_api::ModelEvent::Terminal(
                    harness_model_api::ModelTerminalOutcome::Completed(
                        harness_model_api::ModelCompletion {
                            text: "done".to_string(),
                            usage: None,
                        },
                    ),
                ))
                .unwrap();
        };
        let (drove, ()) = tokio::join!(drive, feed);

        assert!(drove);
        assert_eq!(cancellations.load(Ordering::Relaxed), 0);
        assert_eq!(
            deferred_commands.pop_front(),
            Some(harness_runtime_api::RuntimeCommand::ExecuteCommand {
                text: "/model next-model high".to_string(),
            })
        );
        assert!(suspended_effects.is_some());
        assert!(runtime.queued_steering().is_empty());
        assert!(!runtime.canonical_history().iter().any(|payload| matches!(
            payload,
            SessionPayload::InputMessage { text, .. } if text.starts_with("/model")
        )));
        while let Some(envelope) = runtime_events.try_recv().unwrap() {
            assert!(!matches!(
                &envelope.event,
                harness_runtime_api::RuntimeEvent::SteeringChanged(Some(text))
                    if text.contains("/model")
            ));
            assert!(!matches!(
                &envelope.event,
                harness_runtime_api::RuntimeEvent::Failure(
                    harness_runtime_api::RuntimeFailure {
                        category: harness_runtime_api::RuntimeFailureCategory::Command,
                        ..
                    }
                )
            ));
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edit_command_preserves_exact_replacement_text() {
        assert_eq!(
            parse_edit_message_action("12  leading text\nsecond line").unwrap(),
            EditMessageAction::Replace {
                sequence: 12,
                text: " leading text\nsecond line".to_string(),
            }
        );
        assert_eq!(
            parse_edit_message_action("delete\t12").unwrap(),
            EditMessageAction::Delete { sequence: 12 }
        );
        assert_eq!(
            parse_edit_message_action("").unwrap(),
            EditMessageAction::Choose
        );
    }

    #[test]
    fn edit_command_rejects_missing_text_as_an_ambiguous_delete() {
        let error = parse_edit_message_action("12").unwrap_err();
        assert!(matches!(error, CommandError::Message(_)));
    }

    #[test]
    fn editing_a_message_invalidates_later_derived_context() {
        let records = vec![
            record(
                1,
                SessionPayload::InputMessage {
                    turn_id: 1,
                    text: "first".to_string(),
                },
            ),
            record(
                2,
                SessionPayload::ModelResponseMetadata {
                    turn_id: 1,
                    attempt_id: 1,
                    provider: "provider".to_string(),
                    response_id: "still-valid-prefix".to_string(),
                },
            ),
            record(
                3,
                SessionPayload::AssistantMessage {
                    turn_id: 1,
                    text: "old answer".to_string(),
                },
            ),
            record(
                4,
                SessionPayload::ModelResponseMetadata {
                    turn_id: 1,
                    attempt_id: 2,
                    provider: "provider".to_string(),
                    response_id: "stale-response".to_string(),
                },
            ),
            record(
                5,
                SessionPayload::CompactionCheckpoint {
                    source_revision: 4,
                    summary: "stale summary".to_string(),
                },
            ),
            record(
                6,
                SessionPayload::InputMessage {
                    turn_id: 2,
                    text: "later input".to_string(),
                },
            ),
        ];

        let edited = edit_session_records(&records, 3, Some("new answer\nwith detail")).unwrap();

        assert!(matches!(
            &edited[2],
            SessionPayload::AssistantMessage { text, .. }
                if text == "new answer\nwith detail"
        ));
        assert!(edited.iter().any(|payload| matches!(
            payload,
            SessionPayload::ModelResponseMetadata { response_id, .. }
                if response_id == "still-valid-prefix"
        )));
        assert!(!edited.iter().any(|payload| matches!(
            payload,
            SessionPayload::ModelResponseMetadata { response_id, .. }
                if response_id == "stale-response"
        )));
        assert!(
            !edited
                .iter()
                .any(|payload| matches!(payload, SessionPayload::CompactionCheckpoint { .. }))
        );
        assert!(edited.iter().any(|payload| matches!(
            payload,
            SessionPayload::InputMessage { text, .. } if text == "later input"
        )));
    }

    #[test]
    fn deleting_a_message_removes_only_that_message_and_stale_context() {
        let records = vec![
            record(1, SessionPayload::TurnStarted { turn_id: 1 }),
            record(
                2,
                SessionPayload::InputMessage {
                    turn_id: 1,
                    text: "remove me".to_string(),
                },
            ),
            record(
                3,
                SessionPayload::AssistantMessage {
                    turn_id: 1,
                    text: "retain me".to_string(),
                },
            ),
        ];

        let edited = edit_session_records(&records, 2, None).unwrap();

        assert_eq!(edited.len(), 2);
        assert!(matches!(edited[0], SessionPayload::TurnStarted { .. }));
        assert!(matches!(
            &edited[1],
            SessionPayload::AssistantMessage { text, .. } if text == "retain me"
        ));
    }

    #[test]
    fn tool_activities_are_not_editable_as_raw_messages() {
        let tool_call = record(
            1,
            SessionPayload::ToolCallAccepted {
                turn_id: 7,
                call_id: "call-7".to_owned(),
                invocation: inspect_invocation("src/main.rs"),
                raw_input: SessionToolInput::Freeform("read src/main.rs".to_owned()),
            },
        );

        assert!(editable_message(&tool_call).is_none());
        let error = edit_session_records(&[tool_call], 1, Some("read other.rs")).unwrap_err();
        assert!(error.contains("Typed tool invocations cannot be edited as raw text"));
    }

    #[test]
    fn deleting_a_tool_call_removes_its_finished_activity() {
        let invocation = inspect_invocation("src/main.rs");
        let raw_input = SessionToolInput::Freeform("read src/main.rs".to_owned());
        let records = vec![
            record(
                1,
                SessionPayload::ToolCallAccepted {
                    turn_id: 7,
                    call_id: "call-7".to_owned(),
                    invocation: invocation.clone(),
                    raw_input: raw_input.clone(),
                },
            ),
            record(
                2,
                SessionPayload::ToolExecutionFinished {
                    turn_id: 7,
                    call_id: "call-7".to_owned(),
                    invocation,
                    outcome: inspect_outcome("src/main.rs", &["fn main() {}"]),
                    raw_input,
                    raw_output: "1 bucket fn main() {}".to_owned(),
                },
            ),
            record(
                3,
                SessionPayload::AssistantMessage {
                    turn_id: 7,
                    text: "retained response".to_owned(),
                },
            ),
            record(
                4,
                SessionPayload::ModelResponseMetadata {
                    turn_id: 7,
                    attempt_id: 2,
                    provider: "provider".to_owned(),
                    response_id: "stale-response".to_owned(),
                },
            ),
        ];

        let edited = edit_session_records(&records, 1, None).unwrap();

        assert!(!edited.iter().any(|payload| matches!(
            payload,
            SessionPayload::ToolCallAccepted { call_id, .. }
                | SessionPayload::ToolExecutionFinished { call_id, .. }
                if call_id == "call-7"
        )));
        assert!(edited.iter().any(|payload| matches!(
            payload,
            SessionPayload::AssistantMessage { text, .. } if text == "retained response"
        )));
        assert!(
            !edited
                .iter()
                .any(|payload| matches!(payload, SessionPayload::ModelResponseMetadata { .. }))
        );
    }

    #[test]
    fn resumed_session_coalesces_accepted_and_finished_tool_activity() {
        let root = std::env::temp_dir().join(format!(
            "new-harness-tool-resume-test-{}",
            generate_session_id().unwrap()
        ));
        let invocation = inspect_invocation("src/main.rs");
        let raw_input = SessionToolInput::Freeform("read src/main.rs 1-20".to_owned());
        let payloads = vec![
            SessionPayload::ToolCallAccepted {
                turn_id: 1,
                call_id: "call-1".to_owned(),
                invocation: invocation.clone(),
                raw_input: raw_input.clone(),
            },
            SessionPayload::ToolExecutionFinished {
                turn_id: 1,
                call_id: "call-1".to_owned(),
                invocation,
                outcome: inspect_outcome("src/main.rs", &["fn main() {}"]),
                raw_input,
                raw_output: "1 bucket fn main() {}".to_owned(),
            },
        ];
        let session_id = create_session_fork(&root, &payloads).unwrap();

        let startup = resolve_session_startup(
            &root,
            ResumeSelection::SessionId(session_id.as_str().to_owned()),
        )
        .unwrap();

        assert_eq!(startup.initial_transcript_entries.len(), 1);
        let harness_runtime_api::TranscriptPayload::ToolActivity(activity) =
            &startup.initial_transcript_entries[0].payload
        else {
            panic!("finished tool activity is replayed");
        };
        assert_eq!(activity.call_id, "call-1");
        assert!(matches!(
            activity.invocation,
            harness_tool_api::ToolInvocation::Inspect(harness_tool_api::Prepared::Ready(_))
        ));
        let harness_runtime_api::ToolActivityPhase::Finished {
            outcome,
            raw_output,
        } = &activity.phase
        else {
            panic!("newest tool state is replayed");
        };
        assert!(matches!(outcome, harness_tool_api::ToolOutcome::Inspect(_)));
        assert_eq!(raw_output, "1 bucket fn main() {}");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resumed_finished_only_page_renders_without_an_accepted_record() {
        let root = std::env::temp_dir().join(format!(
            "new-harness-finished-tool-resume-test-{}",
            generate_session_id().unwrap()
        ));
        let payloads = vec![SessionPayload::ToolExecutionFinished {
            turn_id: 1,
            call_id: "call-1".to_owned(),
            invocation: inspect_invocation("src/main.rs"),
            outcome: inspect_outcome("src/main.rs", &["fn main() {}"]),
            raw_input: SessionToolInput::Freeform("read src/main.rs 1-20".to_owned()),
            raw_output: "model-visible output".to_owned(),
        }];
        let session_id = create_session_fork(&root, &payloads).unwrap();

        let startup = resolve_session_startup(
            &root,
            ResumeSelection::SessionId(session_id.as_str().to_owned()),
        )
        .unwrap();

        assert!(matches!(
            &startup.initial_transcript_entries[..],
            [harness_runtime_api::TranscriptSnapshotEntry {
                payload: harness_runtime_api::TranscriptPayload::ToolActivity(
                    harness_runtime_api::ToolActivity {
                        call_id,
                        phase: harness_runtime_api::ToolActivityPhase::Finished { .. },
                        ..
                    }
                ),
                ..
            }] if call_id == "call-1"
        ));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_fork_is_written_with_fresh_monotonic_sequences() {
        let root = std::env::temp_dir().join(format!(
            "new-harness-edit-test-{}",
            generate_session_id().unwrap()
        ));
        let payloads = vec![
            SessionPayload::TurnStarted { turn_id: 1 },
            SessionPayload::InputMessage {
                turn_id: 1,
                text: "edited".to_string(),
            },
        ];

        let session_id = create_session_fork(&root, &payloads).unwrap();
        let records = read_session_records(&root, &session_id).unwrap();

        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            records
                .iter()
                .map(|record| record.payload.clone())
                .collect::<Vec<_>>(),
            payloads
        );
        std::fs::remove_dir_all(root).unwrap();
    }
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
