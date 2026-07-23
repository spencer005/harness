//! OpenResponses wire framing and typed event decoding.
//!
//! The request and stream handling follows the OpenResponses specification:
//! <https://www.openresponses.org/specification>.
//! SSE framing follows the WHATWG server-sent events algorithm.

use harness_model_api::{
    ModelCompletion, ModelEvent, ModelFailure, ModelFailureKind, ModelInput, ModelInterruption,
    ModelMessageRole, ModelReasoning, ModelTerminalOutcome, ModelUsage, ToolCall, ToolInputDelta,
};
use harness_tool_api::{GrammarSyntax, ToolDefinition, ToolInput, ToolInputSchema};
use serde_json::Value;
use thiserror::Error;

/// Default maximum size of one SSE event payload.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 1_048_576;

// WHATWG SSE permits one leading UTF-8 BOM before the first field.
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
// `data:` plus its optional single separating space, per the SSE field grammar.
const MAX_FIELD_PREFIX_BYTES: usize = "data: ".len();

/// One complete server-sent event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Concatenated `data:` field content.
    pub data: String,
}

/// Incremental SSE decoder.
#[derive(Debug)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    max_event_bytes: usize,
    skip_lf_after_cr: bool,
    started: bool,
}

impl SseDecoder {
    /// Creates an SSE decoder with an explicit event-size limit.
    pub fn new(max_event_bytes: usize) -> Result<Self, InvalidEventLimit> {
        if max_event_bytes == 0 {
            return Err(InvalidEventLimit);
        }
        Ok(Self {
            line: Vec::new(),
            data: Vec::new(),
            max_event_bytes,
            skip_lf_after_cr: false,
            started: false,
        })
    }

    /// Feeds bytes and returns every complete event found in the input.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, SseDecodeError> {
        let mut events = Vec::new();

        for &byte in bytes {
            if self.skip_lf_after_cr {
                self.skip_lf_after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }

            match byte {
                // WHATWG defines CRLF, LF, and CR as line endings. Process
                // CR now and suppress only a following LF as the second half
                // of the same CRLF terminator.
                b'\r' => {
                    self.consume_line(&mut events)?;
                    self.skip_lf_after_cr = true;
                }
                b'\n' => self.consume_line(&mut events)?,
                byte => {
                    self.line.push(byte);
                    if self.line.len() > self.max_event_bytes.saturating_add(MAX_FIELD_PREFIX_BYTES)
                    {
                        return Err(SseDecodeError::EventTooLarge {
                            limit: self.max_event_bytes,
                        });
                    }
                }
            }
        }

        Ok(events)
    }

    /// Finishes the stream, discarding any unterminated event as required by SSE.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>, SseDecodeError> {
        let events = Vec::new();
        if !self.line.is_empty() {
            // The WHATWG algorithm discards an event that is not terminated by
            // a blank line when the stream ends.
            self.line.clear();
        }
        self.data.clear();
        Ok(events)
    }

    fn consume_line(&mut self, events: &mut Vec<SseEvent>) -> Result<(), SseDecodeError> {
        let mut line = std::mem::take(&mut self.line);
        if !self.started {
            self.started = true;
            if line.starts_with(UTF8_BOM) {
                line.drain(..UTF8_BOM.len());
            }
        }
        let line = std::str::from_utf8(&line).map_err(SseDecodeError::InvalidUtf8)?;

        if line.is_empty() {
            if !self.data.is_empty() {
                let data = std::mem::take(&mut self.data);
                let data = String::from_utf8(data)
                    .map_err(|error| SseDecodeError::InvalidUtf8(error.utf8_error()))?;
                events.push(SseEvent { data });
            }
            return Ok(());
        }

        if line.starts_with(':') {
            return Ok(());
        }

        let (field, value) = line
            .split_once(':')
            .map(|(field, value)| (field, value.strip_prefix(' ').unwrap_or(value)))
            .unwrap_or((line, ""));

        match field {
            // The typed decoder dispatches on the JSON `type`, so SSE event
            // names and reconnection metadata are intentionally not surfaced.
            "event" | "id" | "retry" => {}
            "data" => {
                let additional_bytes = value.len() + usize::from(!self.data.is_empty());
                if self.data.len() + additional_bytes > self.max_event_bytes {
                    return Err(SseDecodeError::EventTooLarge {
                        limit: self.max_event_bytes,
                    });
                }
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value.as_bytes());
            }
            // WHATWG specifies that unknown fields are ignored.
            _ => {}
        }
        Ok(())
    }
}

/// Failure returned when an SSE event-size limit is zero.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("SSE event-size limit must be greater than zero")]
pub struct InvalidEventLimit;

/// Failure returned by incremental SSE decoding.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SseDecodeError {
    /// An SSE event exceeds the configured byte limit.
    #[error("SSE event exceeds the {limit}-byte limit")]
    EventTooLarge { limit: usize },
    /// A byte sequence is not valid UTF-8.
    #[error("SSE input contains invalid UTF-8")]
    InvalidUtf8(#[source] std::str::Utf8Error),
}

/// Decoder state for one Responses model attempt.
#[derive(Debug)]
pub struct ResponsesEventDecoder {
    sse: SseDecoder,
    assistant_text: String,
    usage: Option<ModelUsage>,
    terminal_seen: bool,
    created_response_id: Option<String>,
    confirmed_response_id: Option<String>,
    /// `item_id -> call_id` learned from `response.output_item.added` frames for
    /// function/custom tool calls. Some providers omit `call_id` on
    /// `function_call_arguments.delta` / `custom_tool_call_input.delta` frames
    /// and only send `item_id`, while the later `response.output_item.done`
    /// frame carries the canonical `call_id`. Mapping back through `item_id`
    /// lets the runtime key tool-input deltas under the same identifier that
    /// the completed `ToolCall` event will use, so the terminal event does not
    /// trip `IncompleteToolInput` on a stale entry.
    tool_call_ids: std::collections::HashMap<String, String>,
}

impl ResponsesEventDecoder {
    /// Creates a decoder with the default event-size limit.
    pub fn new() -> Self {
        Self::with_max_event_bytes(DEFAULT_MAX_EVENT_BYTES)
            .expect("the default event limit is nonzero")
    }

    /// Creates a decoder with an explicit event-size limit.
    pub fn with_max_event_bytes(max_event_bytes: usize) -> Result<Self, InvalidEventLimit> {
        Ok(Self {
            sse: SseDecoder::new(max_event_bytes)?,
            assistant_text: String::new(),
            usage: None,
            terminal_seen: false,
            created_response_id: None,
            confirmed_response_id: None,
            tool_call_ids: std::collections::HashMap::new(),
        })
    }

    /// Feeds network bytes and returns typed model events.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ModelEvent>, ProtocolError> {
        let events = self.sse.push(bytes).map_err(ProtocolError::Sse)?;
        self.decode_events(events)
    }

    /// Finishes the stream and returns final typed events.
    pub fn finish(&mut self) -> Result<Vec<ModelEvent>, ProtocolError> {
        let events = self.sse.finish().map_err(ProtocolError::Sse)?;
        self.decode_events(events)
    }

    fn decode_events(&mut self, events: Vec<SseEvent>) -> Result<Vec<ModelEvent>, ProtocolError> {
        let mut decoded = Vec::new();
        for event in events {
            decoded.extend(self.decode_sse_event(&event.data)?);
        }
        Ok(decoded)
    }

    fn decode_sse_event(&mut self, payload: &str) -> Result<Vec<ModelEvent>, ProtocolError> {
        if payload.trim() == "[DONE]" {
            return Ok(Vec::new());
        }

        let value: Value = serde_json::from_str(payload)?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingEventType)?;

        if matches!(event_type, "response.created" | "response.in_progress") {
            let id = value
                .get("response")
                .and_then(|r| r.get("id"))
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if self.created_response_id.is_none() && id.is_some() {
                self.created_response_id = id.clone();
            }
            let mut events = vec![ModelEvent::Started];
            if let Some(resp_id) = &self.created_response_id {
                events.push(ModelEvent::Metadata(harness_model_api::ModelResponseMetadata {
                    response_id: Some(resp_id.clone()),
                }));
            }
            return Ok(events);
        }

        if event_type == "response.completed" {
            let completed_id = value
                .get("response")
                .and_then(|r| r.get("id"))
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let (Some(created), Some(completed)) = (&self.created_response_id, &completed_id) {
                if created == completed {
                    self.confirmed_response_id = Some(completed.clone());
                } else {
                    self.confirmed_response_id = Some(completed.clone());
                }
            } else if let Some(completed) = completed_id {
                self.confirmed_response_id = Some(completed);
            } else if let Some(created) = &self.created_response_id {
                self.confirmed_response_id = Some(created.clone());
            }

            self.usage = extract_usage(&value);
            let mut events = Vec::new();
            if let Some(resp_id) = &self.confirmed_response_id {
                events.push(ModelEvent::Metadata(harness_model_api::ModelResponseMetadata {
                    response_id: Some(resp_id.clone()),
                }));
            }
            if self.terminal_seen {
                return Err(ProtocolError::DuplicateTerminal);
            }
            self.terminal_seen = true;
            events.push(ModelEvent::Terminal(ModelTerminalOutcome::Completed(
                harness_model_api::ModelCompletion {
                    text: std::mem::take(&mut self.assistant_text),
                    usage: self.usage,
                },
            )));
            return Ok(events);
        }

        if event_type == "response.output_item.added"
            && value
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                == Some("reasoning")
        {
            let item = value
                .get("item")
                .ok_or(ProtocolError::InvalidField { field: "item" })?;
            return Ok(vec![ModelEvent::ReasoningItem(decode_reasoning_item(
                item,
            )?)]);
        }

        // Remember the canonical call_id that a tool item will use on its
        // `output_item.done` frame. Delta frames for the same call frequently
        // carry only `item_id`, so we map `item_id -> call_id` here and resolve
        // deltas against it below.
        if event_type == "response.output_item.added"
            && matches!(
                value
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str),
                Some("function_call" | "custom_tool_call")
            )
        {
            if let Some(item) = value.get("item") {
                if let (Some(item_id), Some(call_id)) = (
                    item.get("id").and_then(Value::as_str),
                    item.get("call_id").and_then(Value::as_str),
                ) {
                    self.tool_call_ids
                        .insert(item_id.to_owned(), call_id.to_owned());
                }
            }
            // Fall through; lifecycle-only event for tool calls.
        }

        if event_type == "response.content_part.added"
            && value
                .get("part")
                .and_then(|part| part.get("type"))
                .and_then(Value::as_str)
                == Some("reasoning_text")
        {
            let part = value
                .get("part")
                .ok_or(ProtocolError::InvalidField { field: "part" })?;
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            if !text.is_empty() {
                return Ok(vec![ModelEvent::ReasoningContentDelta(text.to_owned())]);
            }
        }

        // OpenResponses emits item and content lifecycle events around the
        // semantic deltas. The runtime consumes lifecycle-only events after
        // preserving any reasoning item data carried by them.
        if matches!(
            event_type,
            "response.created"
                | "response.in_progress"
                | "response.metadata"
                | "response.output_item.added"
                | "response.content_part.added"
                | "response.content_part.done"
                | "response.output_text.done"
                | "response.reasoning_summary_part.added"
                | "response.reasoning_summary_text.done"
                | "response.reasoning_summary_part.done"
                | "response.reasoning_text.done"
                | "response.function_call_arguments.done"
                | "response.custom_tool_call_input.done"
                | "codex.rate_limits"
        ) || event_type.starts_with("codex.") {
            return Ok(Vec::new());
        }

        if event_type == "response.output_item.done"
            && !matches!(
                value
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str),
                Some("function_call" | "custom_tool_call" | "reasoning")
            )
        {
            return Ok(Vec::new());
        }

        let event = match decode_event_value(&value, &mut self.assistant_text, &mut self.usage) {
            Ok(event) => event,
            Err(ProtocolError::UnsupportedEvent(_)) if event_type.contains(':') => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };

        // Normalize tool-call identifiers so delta fragments and the completed
        // `ToolCall` event share the same key.
        let mut event = event;
        if let ModelEvent::ToolInputDelta(delta) = &event {
            if let Some(call_id) = self.tool_call_ids.get(&delta.call_id) {
                let mut delta = delta.clone();
                delta.call_id = call_id.clone();
                event = ModelEvent::ToolInputDelta(delta);
            }
        }
        if matches!(event, ModelEvent::Terminal(_)) {
            if self.terminal_seen {
                return Err(ProtocolError::DuplicateTerminal);
            }
            self.terminal_seen = true;
        }
        Ok(vec![event])
    }
}

impl Default for ResponsesEventDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Failure returned by Responses protocol decoding.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// SSE framing fails.
    #[error(transparent)]
    Sse(#[from] SseDecodeError),
    /// A JSON payload cannot be decoded.
    #[error("Responses event is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A payload omits its event type.
    #[error("Responses event omits its type")]
    MissingEventType,
    /// A payload uses an unsupported event type.
    #[error("unsupported Responses event type: {0}")]
    UnsupportedEvent(String),
    /// A required event field is absent or has the wrong type.
    #[error("Responses event field `{field}` is missing or invalid")]
    InvalidField { field: &'static str },
    /// More than one terminal outcome appears in one stream.
    #[error("Responses stream contains more than one terminal outcome")]
    DuplicateTerminal,
    /// A tool's serialized JSON schema is invalid.
    #[error("tool `{name}` has an invalid JSON schema: {reason}")]
    InvalidToolSchema { name: String, reason: String },
}

/// Decodes one complete Responses JSON event.
pub fn decode_event(
    payload: &str,
    assistant_text: &mut String,
    usage: &mut Option<ModelUsage>,
) -> Result<ModelEvent, ProtocolError> {
    let value: Value = serde_json::from_str(payload)?;
    decode_event_value(&value, assistant_text, usage)
}

fn decode_event_value(
    value: &Value,
    assistant_text: &mut String,
    usage: &mut Option<ModelUsage>,
) -> Result<ModelEvent, ProtocolError> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::MissingEventType)?;

    match event_type {
        "response.created" | "response.in_progress" => Ok(ModelEvent::Started),
        "response.output_text.delta" => {
            let delta = string_field(&value, "delta")?;
            assistant_text.push_str(delta);
            Ok(ModelEvent::AssistantTextDelta(delta.to_owned()))
        }
        "response.reasoning_summary_text.delta" => Ok(ModelEvent::ReasoningSummaryDelta(
            string_field(&value, "delta")?.to_owned(),
        )),
        "response.reasoning_text.delta" => Ok(ModelEvent::ReasoningContentDelta(
            string_field(&value, "delta")?.to_owned(),
        )),
        "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
            Ok(ModelEvent::ToolInputDelta(ToolInputDelta {
                call_id: value
                    .get("call_id")
                    .or_else(|| value.get("item_id"))
                    .and_then(Value::as_str)
                    .ok_or(ProtocolError::InvalidField { field: "call_id" })?
                    .to_owned(),
                fragment: string_field(&value, "delta")?.to_owned(),
            }))
        }
        "response.output_item.done" => decode_output_item(&value),
        "response.completed" => {
            *usage = extract_usage(&value);
            Ok(ModelEvent::Terminal(ModelTerminalOutcome::Completed(
                ModelCompletion {
                    text: std::mem::take(assistant_text),
                    usage: *usage,
                },
            )))
        }
        "response.incomplete" => Ok(ModelEvent::Terminal(ModelTerminalOutcome::Interrupted(
            ModelInterruption {
                reason: string_field(&value, "reason")
                    .unwrap_or("response became incomplete")
                    .to_owned(),
            },
        ))),
        "response.failed" => Ok(ModelEvent::Terminal(ModelTerminalOutcome::Failed(
            ModelFailure {
                kind: ModelFailureKind::ProviderRejected,
                message: value
                    .get("error")
                    .or_else(|| {
                        value
                            .get("response")
                            .and_then(|response| response.get("error"))
                    })
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Responses provider reported a failure")
                    .to_owned(),
            },
        ))),
        "response.cancelled" => Ok(ModelEvent::Terminal(ModelTerminalOutcome::Cancelled(
            harness_model_api::ModelCancellation {
                reason: "Responses provider cancelled the response".to_owned(),
            },
        ))),
        _ => Err(ProtocolError::UnsupportedEvent(event_type.to_owned())),
    }
}

fn decode_output_item(value: &Value) -> Result<ModelEvent, ProtocolError> {
    let item = value
        .get("item")
        .ok_or(ProtocolError::InvalidField { field: "item" })?;

    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .ok_or(ProtocolError::InvalidField { field: "call_id" })?
                .to_owned();
            Ok(ModelEvent::ToolCall(ToolCall {
                call_id,
                name: string_field(item, "name")?.to_owned(),
                input: ToolInput::FunctionJson(string_field(item, "arguments")?.to_owned()),
            }))
        }
        Some("custom_tool_call") => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .ok_or(ProtocolError::InvalidField { field: "call_id" })?
                .to_owned();
            Ok(ModelEvent::ToolCall(ToolCall {
                call_id,
                name: string_field(item, "name")?.to_owned(),
                input: ToolInput::Freeform(string_field(item, "input")?.to_owned()),
            }))
        }
        Some("reasoning") => Ok(ModelEvent::ReasoningItem(decode_reasoning_item(item)?)),
        Some(item_type) => Err(ProtocolError::UnsupportedEvent(item_type.to_owned())),
        None => Err(ProtocolError::InvalidField { field: "item.type" }),
    }
}

fn decode_reasoning_item(item: &Value) -> Result<ModelReasoning, ProtocolError> {
    Ok(ModelReasoning {
        content: reasoning_text_array(item, "content")?,
        encrypted_content: item
            .get("encrypted_content")
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string())
            }),
        summary: reasoning_text_array(item, "summary")?,
    })
}

fn reasoning_text_array(
    item: &Value,
    field: &'static str,
) -> Result<Option<String>, ProtocolError> {
    let Some(value) = item.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    if let Some(text) = value.as_str() {
        return Ok(Some(text.to_owned()));
    }
    let Some(parts) = value.as_array() else {
        return Err(ProtocolError::InvalidField { field });
    };
    let mut text = String::new();
    for part in parts {
        if let Some(part_text) = part.get("text").and_then(Value::as_str) {
            text.push_str(part_text);
        }
    }
    Ok(Some(text))
}

fn string_field<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, ProtocolError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(ProtocolError::InvalidField { field })
}

/// Encodes canonical model input as Responses items.
pub fn encode_input(
    input: &[ModelInput],
    developer_role_support: harness_model_api::DeveloperRoleSupport,
    allow_multiple_system_messages: bool,
) -> Vec<sonic_rs::Value> {
    let mut seen_system_or_developer = false;
    input
        .iter()
        .map(|item| match item {
            ModelInput::Message { role, text } => {
                let role = match role {
                    ModelMessageRole::System => {
                        if !allow_multiple_system_messages && seen_system_or_developer {
                            "user"
                        } else {
                            seen_system_or_developer = true;
                            match developer_role_support {
                                harness_model_api::DeveloperRoleSupport::Disabled => "system",
                                harness_model_api::DeveloperRoleSupport::Supported
                                | harness_model_api::DeveloperRoleSupport::DeveloperOnly => "developer",
                            }
                        }
                    }
                    ModelMessageRole::Developer => {
                        if !allow_multiple_system_messages && seen_system_or_developer {
                            "user"
                        } else {
                            seen_system_or_developer = true;
                            "developer"
                        }
                    }
                    ModelMessageRole::User => "user",
                    ModelMessageRole::Assistant => "assistant",
                };
                let content_type = match role {
                    "assistant" => "output_text",
                    _ => "input_text",
                };
                sonic_rs::json!({
                    "type": "message",
                    "role": role,
                    "content": [{ "type": content_type, "text": text }]
                })
            }
            ModelInput::AssistantToolCall {
                call_id,
                name,
                arguments,
            } => sonic_rs::json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            }),
            ModelInput::FreeformToolCall {
                call_id,
                name,
                input,
            } => sonic_rs::json!({
                "type": "custom_tool_call",
                "call_id": call_id,
                "name": name,
                "input": input,
            }),
            ModelInput::ToolResult { call_id, output } => sonic_rs::json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output
            }),
            ModelInput::FreeformToolResult { call_id, output } => sonic_rs::json!({
                "type": "custom_tool_call_output",
                "call_id": call_id,
                "output": output
            }),
            ModelInput::Reasoning {
                content,
                encrypted_content,
                summary,
            } => {
                let summary_val = match summary {
                    Some(summary_text) => sonic_rs::json!([{
                        "type": "summary_text",
                        "text": summary_text,
                    }]),
                    None => sonic_rs::json!([]),
                };
                let mut item = sonic_rs::json!({
                    "type": "reasoning",
                    "summary": summary_val,
                });
                if let Some(content) = content {
                    item["content"] = sonic_rs::json!([{
                        "type": "reasoning_text",
                        "text": content,
                    }]);
                }
                if let Some(encrypted_content) = encrypted_content {
                    item["encrypted_content"] = sonic_rs::json!(encrypted_content);
                }
                item
            }
        })
        .collect()
}

/// Encodes canonical tool definitions using the OpenResponses function and custom-tool formats.
pub fn encode_tools(tools: &[ToolDefinition]) -> Result<Vec<sonic_rs::Value>, ProtocolError> {
    tools
        .iter()
        .map(|tool| {
            let parameters: sonic_rs::Value = match &tool.input_schema {
                ToolInputSchema::JsonSchema(schema) => sonic_rs::from_str(schema.as_str())
                    .map_err(|error| ProtocolError::InvalidToolSchema {
                        name: tool.name.as_str().to_owned(),
                        reason: error.to_string(),
                    })?,
                ToolInputSchema::FreeformGrammar { syntax, definition } => {
                    let syntax = match syntax {
                        GrammarSyntax::Regex => "regex",
                        GrammarSyntax::Lark => "lark",
                    };
                    return Ok(sonic_rs::json!({
                        "type": "custom",
                        "name": tool.name.as_str(),
                        "description": tool.description,
                        "format": {
                            "type": "grammar",
                            "syntax": syntax,
                            "definition": definition
                        }
                    }));
                }
            };

            Ok(sonic_rs::json!({
                "type": "function",
                "name": tool.name.as_str(),
                "description": tool.description,
                "parameters": parameters
            }))
        })
        .collect()
}

fn extract_usage(value: &Value) -> Option<ModelUsage> {
    let usage = value.get("response")?.get("usage")?;
    Some(ModelUsage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_decoder_accepts_byte_split_event() {
        let mut decoder = SseDecoder::new(128).unwrap();
        assert!(decoder.push(b"da").unwrap().is_empty());
        assert!(
            decoder
                .push(b"ta: {\"type\":\"response.created\"}\r\n\r\n")
                .unwrap()
                == vec![SseEvent {
                    data: "{\"type\":\"response.created\"}".to_owned(),
                }]
        );
    }

    #[test]
    fn sse_decoder_joins_data_lines_and_ignores_comments() {
        let mut decoder = SseDecoder::new(128).unwrap();
        let events = decoder
            .push(b": keepalive\r\ndata: first\r\ndata: second\r\n\r\n")
            .unwrap();
        assert_eq!(
            events,
            vec![SseEvent {
                data: "first\nsecond".to_owned()
            }]
        );
    }

    #[test]
    fn sse_decoder_ignores_event_metadata() {
        let mut decoder = SseDecoder::new(128).unwrap();
        let events = decoder
            .push(b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n")
            .unwrap();
        assert_eq!(
            events,
            vec![SseEvent {
                data: "{\"type\":\"response.created\"}".to_owned()
            }]
        );
    }

    #[test]
    fn sse_decoder_accepts_all_standard_line_endings_and_unknown_fields() {
        let mut decoder = SseDecoder::new(128).unwrap();
        let events = decoder
            .push(b"unknown: ignored\rdata: one\rdata: two\r\r")
            .unwrap();
        assert_eq!(
            events,
            vec![SseEvent {
                data: "one\ntwo".to_owned()
            }]
        );
    }

    #[test]
    fn event_decoder_keeps_terminal_outcomes_distinct() {
        let mut decoder = ResponsesEventDecoder::new();
        let events = decoder
            .push(b"data: {\"type\":\"response.incomplete\",\"reason\":\"user interrupt\"}\n\n")
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ModelEvent::Terminal(ModelTerminalOutcome::Interrupted(_))]
        ));
    }

    #[test]
    fn lifecycle_events_do_not_emit_runtime_events() {
        let mut decoder = ResponsesEventDecoder::new();
        assert!(decoder
            .push(b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n\n")
            .unwrap()
            .is_empty());
        assert!(
            decoder
                .push(b"data: {\"type\":\"response.content_part.added\"}\n\n")
                .unwrap()
                .is_empty()
        );
        assert!(decoder
            .push(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"status\":\"completed\"}}\n\n")
            .unwrap()
            .is_empty());
        assert!(
            decoder
                .push(b"data: {\"type\":\"vendor:trace\",\"sequence_number\":1}\n\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn done_sentinel_does_not_become_completion() {
        let mut decoder = ResponsesEventDecoder::new();
        assert!(decoder.push(b"data: [DONE]\n\n").unwrap().is_empty());
    }

    #[test]
    fn duplicate_terminal_outcomes_are_rejected() {
        let mut decoder = ResponsesEventDecoder::new();
        let payload = b"data: {\"type\":\"response.incomplete\"}\n\n";
        decoder.push(payload).unwrap();
        assert!(matches!(
            decoder.push(payload),
            Err(ProtocolError::DuplicateTerminal)
        ));
    }

    /// Regression: some Responses providers emit `function_call_arguments.delta`
    /// (or `custom_tool_call_input.delta`) frames carrying only `item_id`, then
    /// announce the canonical `call_id` on the `output_item.done` frame. The
    /// decoder must rewrite the delta's key to the canonical `call_id` learned
    /// from the earlier `output_item.added` frame, so the runtime can pair the
    /// accumulated input with the completed `ToolCall` and not trip
    /// `IncompleteToolInput` on the terminal event.
    #[test]
    fn tool_input_delta_keyed_by_item_id_is_remapped_to_call_id() {
        let mut decoder = ResponsesEventDecoder::new();
        // `added` carries both `id` (item_id) and `call_id`.
        decoder
            .push(b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc-1\",\"call_id\":\"call-xyz\",\"name\":\"lookup\"}}\n\n")
            .unwrap();
        // Delta carries only `item_id`.
        let events = decoder
            .push(b"data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc-1\",\"delta\":\"{\\\"q\\\":\\\"\"}\n\n")
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ModelEvent::ToolInputDelta(delta)] if delta.call_id == "call-xyz"
                && delta.fragment == "{\"q\":\""
        ));
        // Done carries the canonical `call_id`.
        let events = decoder
            .push(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc-1\",\"call_id\":\"call-xyz\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"hi\\\"}\"}}\n\n")
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ModelEvent::ToolCall(call)] if call.call_id == "call-xyz"
        ));
    }

    /// Regression: a provider that never emits `call_id` anywhere keys both the
    /// deltas and the completed `ToolCall` off `id`. The decoder must keep them
    /// aligned so the runtime can clear the pending input on the done frame.
    #[test]
    fn tool_call_without_call_id_falls_back_to_item_id() {
        let mut decoder = ResponsesEventDecoder::new();
        let events = decoder
            .push(b"data: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"ctc-1\",\"delta\":\"command: \"}\n\n")
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ModelEvent::ToolInputDelta(delta)] if delta.call_id == "ctc-1"
        ));
        let events = decoder
            .push(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"id\":\"ctc-1\",\"name\":\"shell\",\"input\":\"command: ls\"}}\n\n")
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ModelEvent::ToolCall(call)] if call.call_id == "ctc-1"
        ));
    }

    #[test]
    fn event_size_limit_is_enforced_before_buffer_growth() {
        let mut decoder = SseDecoder::new(4).unwrap();
        assert!(matches!(
            decoder.push(b"data: five!\n"),
            Err(SseDecodeError::EventTooLarge { limit: 4 })
        ));
    }

    #[test]
    fn unterminated_field_line_is_bounded() {
        let mut decoder = SseDecoder::new(4).unwrap();
        assert!(matches!(
            decoder.push(b"data:123456"),
            Err(SseDecodeError::EventTooLarge { limit: 4 })
        ));
    }

    #[test]
    fn encodes_ollama_responses_message_items() {
        let input = vec![
            ModelInput::Message {
                role: ModelMessageRole::User,
                text: "hello".to_owned(),
            },
            ModelInput::Message {
                role: ModelMessageRole::Assistant,
                text: "hi".to_owned(),
            },
            ModelInput::AssistantToolCall {
                call_id: "call-1".to_owned(),
                name: "lookup".to_owned(),
                arguments: "{}".to_owned(),
            },
            ModelInput::ToolResult {
                call_id: "call-1".to_owned(),
                output: "result".to_owned(),
            },
        ];

        let encoded = encode_input(
            &input,
            harness_model_api::DeveloperRoleSupport::Disabled,
            true,
        );
        assert_eq!(encoded[0]["type"], "message");
        assert_eq!(encoded[0]["content"][0]["type"], "input_text");
        assert_eq!(encoded[1]["content"][0]["type"], "output_text");
        assert_eq!(encoded[2]["type"], "function_call");
        assert_eq!(encoded[3]["type"], "function_call_output");
    }

    #[test]
    fn reasoning_item_spec_compliance_and_roundtrip() {
        let input = vec![ModelInput::Reasoning {
            content: Some("raw reasoning step".to_string()),
            encrypted_content: Some("enc_payload_xyz".to_string()),
            summary: Some("reasoning summary".to_string()),
        }];

        let encoded = encode_input(
            &input,
            harness_model_api::DeveloperRoleSupport::Disabled,
            true,
        );

        assert_eq!(encoded[0]["type"], "reasoning");
        assert_eq!(encoded[0]["content"][0]["type"], "reasoning_text");
        assert_eq!(encoded[0]["content"][0]["text"], "raw reasoning step");
        assert_eq!(encoded[0]["encrypted_content"], "enc_payload_xyz");
        assert_eq!(encoded[0]["summary"][0]["type"], "summary_text");
        assert_eq!(encoded[0]["summary"][0]["text"], "reasoning summary");

        // Verify decoding
        let item_wrapper = sonic_rs::json!({
            "type": "response.output_item.done",
            "item": encoded[0]
        });
        let mut text = String::new();
        let mut usage = None;
        let event = decode_event(&item_wrapper.to_string(), &mut text, &mut usage).unwrap();
        if let ModelEvent::ReasoningItem(reasoning) = event {
            assert_eq!(reasoning.content.as_deref(), Some("raw reasoning step"));
            assert_eq!(reasoning.encrypted_content.as_deref(), Some("enc_payload_xyz"));
            assert_eq!(reasoning.summary.as_deref(), Some("reasoning summary"));
        } else {
            panic!("Expected ReasoningItem event");
        }
    }

    #[test]
    fn rejects_invalid_tool_schema() {
        let tool = ToolDefinition {
            name: harness_tool_api::ToolName::new("lookup").unwrap(),
            description: "Look up a value".to_owned(),
            input_schema: ToolInputSchema::JsonSchema(harness_tool_api::JsonSchema::new(
                "not json",
            )),
            capabilities: Default::default(),
        };

        assert!(matches!(
            encode_tools(&[tool]),
            Err(ProtocolError::InvalidToolSchema { .. })
        ));
    }

    #[test]
    fn test_captures_and_confirms_response_id() {
        let mut decoder = ResponsesEventDecoder::new();
        let events = decoder
            .push(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_abc123\"}}\n\n")
            .unwrap();
        assert_eq!(
            events,
            vec![
                ModelEvent::Started,
                ModelEvent::Metadata(harness_model_api::ModelResponseMetadata {
                    response_id: Some("resp_abc123".to_owned()),
                })
            ]
        );

        let events = decoder
            .push(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_abc123\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n")
            .unwrap();
        assert_eq!(
            events,
            vec![
                ModelEvent::Metadata(harness_model_api::ModelResponseMetadata {
                    response_id: Some("resp_abc123".to_owned()),
                }),
                ModelEvent::Terminal(ModelTerminalOutcome::Completed(ModelCompletion {
                    text: "".to_owned(),
                    usage: Some(ModelUsage {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                    }),
                }))
            ]
        );
    }
}
