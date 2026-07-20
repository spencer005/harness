//! OpenAI Chat Completions request encoding and incremental stream decoding.

use std::collections::BTreeMap;

use harness_model_api::{
    ModelAttempt, ModelCompletion, ModelEvent, ModelFailure, ModelFailureKind, ModelInput,
    ModelMessageRole, ModelTerminalOutcome, ModelUsage, ToolCall, ToolInputDelta,
};
use harness_responses_protocol::{InvalidEventLimit, SseDecodeError, SseDecoder};
use harness_tool_api::{ToolInput, ToolInputSchema};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value, json};
use thiserror::Error;

/// Default maximum size of one Chat Completions SSE event.
pub const DEFAULT_MAX_EVENT_BYTES: usize = harness_responses_protocol::DEFAULT_MAX_EVENT_BYTES;

/// Encodes one immutable semantic attempt as an OpenAI Chat Completions request body.
pub fn encode_request(attempt: &ModelAttempt) -> Result<Value, ChatProtocolError> {
    let request = &attempt.request;
    let messages = encode_messages(&request.input)?;
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let parameters = match &tool.input_schema {
                ToolInputSchema::JsonSchema(schema) => {
                    sonic_rs::from_str(schema.as_str())
                        .map_err(ChatProtocolError::InvalidToolSchema)?
                }
                ToolInputSchema::FreeformGrammar {
                    syntax,
                    definition,
                } => json!({
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
                }),
            };
            Ok(json!({
                "type": "function",
                "function": {
                    "name": tool.name.as_str(),
                    "description": tool.description,
                    "parameters": parameters
                }
            }))
        })
        .collect::<Result<Vec<_>, ChatProtocolError>>()?;

    let mut body_map = BTreeMap::new();
    body_map.insert("model".to_string(), json!(request.selection.model.clone()));
    body_map.insert("messages".to_string(), json!(messages));
    body_map.insert("stream".to_string(), json!(true));
    body_map.insert(
        "stream_options".to_string(),
        json!({ "include_usage": true }),
    );

    if !tools.is_empty() {
        body_map.insert("tools".to_string(), json!(tools));
    }
    if let Some(reasoning_effort) = &request.selection.reasoning_effort {
        body_map.insert(
            "reasoning_effort".to_string(),
            json!(reasoning_effort.clone()),
        );
    }
    if let Some(service_tier) = &request.selection.service_tier {
        body_map.insert("service_tier".to_string(), json!(service_tier.clone()));
    }

    Ok(json!(body_map))
}

fn encode_messages(input: &[ModelInput]) -> Result<Vec<Value>, ChatProtocolError> {
    let mut messages = Vec::new();
    let mut index = 0;
    while index < input.len() {
        match &input[index] {
            ModelInput::Message { role, text } => {
                let role = match role {
                    ModelMessageRole::Developer => "developer",
                    ModelMessageRole::User => "user",
                    ModelMessageRole::Assistant => "assistant",
                };
                messages.push(json!({ "role": role, "content": text }));
                index += 1;
            }
            ModelInput::AssistantToolCall { .. } => {
                let mut calls = Vec::new();
                while let Some(ModelInput::AssistantToolCall {
                    call_id,
                    name,
                    arguments,
                }) = input.get(index)
                {
                    calls.push(json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments
                        }
                    }));
                    index += 1;
                }
                messages.push(json!({
                    "role": "assistant",
                     "content": json!(null),
                     "tool_calls": calls

                }));
            }
            ModelInput::ToolResult { call_id, output } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output
                }));
                index += 1;
            }
            ModelInput::FreeformToolCall { .. } | ModelInput::FreeformToolResult { .. } => {
                return Err(ChatProtocolError::UnsupportedFreeformInput);
            }
            ModelInput::Reasoning { .. } => {
                return Err(ChatProtocolError::UnsupportedReasoningInput);
            }
        }
    }
    Ok(messages)
}

/// Incrementally decodes one streamed Chat Completions response.
#[derive(Debug)]
pub struct ChatEventDecoder {
    sse: SseDecoder,
    assistant_text: String,
    usage: Option<ModelUsage>,
    response_id: Option<String>,
    finish_reason: Option<String>,
    freeform_tools: std::collections::BTreeSet<String>,
    tool_calls: BTreeMap<u64, PartialToolCall>,
    started: bool,
    terminal_seen: bool,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ChatEventDecoder {
    /// Creates a decoder without advertised freeform tools.
    pub fn new() -> Self {
        Self::with_tools(DEFAULT_MAX_EVENT_BYTES, &[]).expect("the default event limit is nonzero")
    }

    /// Creates a decoder for the exact tools advertised by one request.
    pub fn with_tools(
        max_event_bytes: usize,
        tools: &[harness_tool_api::ToolDefinition],
    ) -> Result<Self, InvalidEventLimit> {
        let freeform_tools = tools
            .iter()
            .filter(|tool| matches!(tool.input_schema, ToolInputSchema::FreeformGrammar { .. }))
            .map(|tool| tool.name.as_str().to_owned())
            .collect();
        Ok(Self {
            sse: SseDecoder::new(max_event_bytes)?,
            assistant_text: String::new(),
            usage: None,
            response_id: None,
            finish_reason: None,
            freeform_tools,
            tool_calls: BTreeMap::new(),
            started: false,
            terminal_seen: false,
        })
    }

    /// Feeds bytes and returns every complete typed event.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ModelEvent>, ChatProtocolError> {
        let events = self.sse.push(bytes)?;
        self.decode_sse_events(events)
    }

    /// Finishes framing and rejects a stream without the `[DONE]` sentinel.
    pub fn finish(&mut self) -> Result<Vec<ModelEvent>, ChatProtocolError> {
        let events = self.sse.finish()?;
        let decoded = self.decode_sse_events(events)?;
        if !self.terminal_seen {
            return Err(ChatProtocolError::MissingDone);
        }
        Ok(decoded)
    }

    fn decode_sse_events(
        &mut self,
        events: Vec<harness_responses_protocol::SseEvent>,
    ) -> Result<Vec<ModelEvent>, ChatProtocolError> {
        let mut decoded = Vec::new();
        for event in events {
            if event.data.trim() == "[DONE]" {
                decoded.extend(self.complete()?);
            } else {
                decoded.extend(self.decode_chunk(&event.data)?);
            }
        }
        Ok(decoded)
    }

    fn decode_chunk(&mut self, payload: &str) -> Result<Vec<ModelEvent>, ChatProtocolError> {
        if self.terminal_seen {
            return Err(ChatProtocolError::EventAfterTerminal);
        }
        let value: Value = sonic_rs::from_str(payload)?;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("OpenAI Chat API reported an error");
            self.terminal_seen = true;
            return Ok(vec![ModelEvent::Terminal(ModelTerminalOutcome::Failed(
                ModelFailure {
                    kind: ModelFailureKind::ProviderRejected,
                    message: message.to_owned(),
                },
            ))]);
        }

        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(ModelEvent::Started);
        }
        if self.response_id.is_none() {
            self.response_id = value.get("id").and_then(Value::as_str).map(str::to_owned);
            if self.response_id.is_some() {
                events.push(ModelEvent::Metadata(
                    harness_model_api::ModelResponseMetadata {
                        response_id: self.response_id.clone(),
                    },
                ));
            }
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            let usage = ModelUsage {
                input_tokens: usage.get("prompt_tokens").and_then(Value::as_u64),
                output_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
            };
            self.usage = Some(usage);
            events.push(ModelEvent::Usage(usage));
        }

        for choice in value
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_owned());
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                self.assistant_text.push_str(content);
                events.push(ModelEvent::AssistantTextDelta(content.to_owned()));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(ChatProtocolError::InvalidField("tool_calls.index"))?;
                let partial = self.tool_calls.entry(index).or_default();
                if let Some(call_id) = call.get("id").and_then(Value::as_str) {
                    partial.call_id = Some(call_id.to_owned());
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        partial.name = Some(name.to_owned());
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        let call_id = partial
                            .call_id
                            .clone()
                            .ok_or(ChatProtocolError::InvalidField("tool_calls.id"))?;
                        partial.arguments.push_str(arguments);
                        events.push(ModelEvent::ToolInputDelta(ToolInputDelta {
                            call_id,
                            fragment: arguments.to_owned(),
                        }));
                    }
                }
            }
        }
        Ok(events)
    }

    fn complete(&mut self) -> Result<Vec<ModelEvent>, ChatProtocolError> {
        if self.terminal_seen {
            return Err(ChatProtocolError::DuplicateTerminal);
        }
        self.terminal_seen = true;
        let reason = self.finish_reason.as_deref().unwrap_or("stop");
        if !matches!(reason, "stop" | "tool_calls") {
            return Ok(vec![ModelEvent::Terminal(ModelTerminalOutcome::Failed(
                ModelFailure {
                    kind: ModelFailureKind::ProviderRejected,
                    message: format!("OpenAI Chat API finished with reason {reason:?}"),
                },
            ))]);
        }

        let mut events = Vec::new();
        for (_, partial) in std::mem::take(&mut self.tool_calls) {
            let call_id = partial
                .call_id
                .ok_or(ChatProtocolError::InvalidField("tool_calls.id"))?;
            let name = partial
                .name
                .ok_or(ChatProtocolError::InvalidField("tool_calls.function.name"))?;
            let input = if self.freeform_tools.contains(&name) {
                let wrapped: Value = sonic_rs::from_str(&partial.arguments)
                    .map_err(ChatProtocolError::InvalidFreeformArguments)?;
                let input = wrapped.get("input").and_then(Value::as_str).ok_or(
                    ChatProtocolError::InvalidField("tool_calls.function.arguments.input"),
                )?;
                ToolInput::Freeform(input.to_owned())
            } else {
                ToolInput::FunctionJson(partial.arguments)
            };
            events.push(ModelEvent::ToolCall(ToolCall {
                call_id,
                name,
                input,
            }));
        }
        events.push(ModelEvent::Terminal(ModelTerminalOutcome::Completed(
            ModelCompletion {
                text: std::mem::take(&mut self.assistant_text),
                usage: self.usage,
            },
        )));
        Ok(events)
    }
}

impl Default for ChatEventDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// OpenAI Chat request or stream protocol failure.
#[derive(Debug, Error)]
pub enum ChatProtocolError {
    /// SSE framing fails.
    #[error(transparent)]
    Sse(#[from] SseDecodeError),
    /// JSON parsing fails.
    #[error("JSON parsing fails: {0}")]
    Json(#[from] sonic_rs::Error),
    /// A required field is absent or invalid.
    #[error("OpenAI Chat field `{0}` is missing or invalid")]
    InvalidField(&'static str),
    /// A wrapped freeform call does not contain the exact string input contract.
    #[error("OpenAI Chat freeform compatibility arguments are not valid JSON: {0}")]
    InvalidFreeformArguments(sonic_rs::Error),
    /// A function tool contains an invalid JSON schema.
    #[error("OpenAI Chat function tool schema is not valid JSON: {0}")]
    InvalidToolSchema(sonic_rs::Error),
    /// Native freeform input requires the Responses custom-tool format.
    #[error("OpenAI Chat cannot encode native freeform input")]
    UnsupportedFreeformInput,
    /// Responses reasoning items do not have a Chat Completions input format.
    #[error("OpenAI Chat cannot encode Responses reasoning items")]
    UnsupportedReasoningInput,
    /// The stream closes without the required terminal sentinel.
    #[error("OpenAI Chat stream ended without [DONE]")]
    MissingDone,
    /// More than one terminal marker is received.
    #[error("OpenAI Chat stream contains more than one terminal marker")]
    DuplicateTerminal,
    /// Data arrives after a terminal outcome.
    #[error("OpenAI Chat stream contains data after its terminal outcome")]
    EventAfterTerminal,
}
