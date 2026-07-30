//! OpenAPI-compatible Chat Completions request encoding and incremental stream decoding.

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

/// Encodes one immutable semantic attempt as an OpenAPI-compatible Chat Completions request body.
pub fn encode_request(attempt: &ModelAttempt) -> Result<Value, ChatCompletionsProtocolError> {
    let request = &attempt.request;
    let messages = encode_messages(&request.input)?;
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let description = tool.function_compatibility_description();
            let parameters = match &tool.input_schema {
                ToolInputSchema::JsonSchema(schema) => {
                    sonic_rs::from_str(schema.as_str())
                        .map_err(ChatCompletionsProtocolError::InvalidToolSchema)?
                }
                ToolInputSchema::FreeformGrammar { .. } => json!({
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "Complete raw tool input, passed as one JSON string exactly as specified by the function description."
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
                    "description": description,
                    "parameters": parameters
                }
            }))
        })
        .collect::<Result<Vec<_>, ChatCompletionsProtocolError>>()?;

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

fn encode_messages(input: &[ModelInput]) -> Result<Vec<Value>, ChatCompletionsProtocolError> {
    let mut messages = Vec::new();
    let mut index = 0;
    let mut pending_reasoning: Option<(Option<String>, Option<String>)> = None;
    while index < input.len() {
        match &input[index] {
            ModelInput::Message { role, text } => {
                let role = match role {
                    ModelMessageRole::System => "system",
                    ModelMessageRole::Developer => "developer",
                    ModelMessageRole::User => "user",
                    ModelMessageRole::Assistant => "assistant",
                };
                let mut message = json!({ "role": role, "content": text });
                if role == "assistant" {
                    if let Some((content, summary)) = pending_reasoning.take() {
                        if let Some(content) = content {
                            message["reasoning_content"] = json!(content);
                        }
                        if let Some(summary) = summary {
                            message["reasoning_summary"] = json!(summary);
                        }
                    }
                }
                messages.push(message);
                index += 1;
            }
            ModelInput::AssistantToolCall { .. } | ModelInput::FreeformToolCall { .. } => {
                let mut calls = Vec::new();
                while let Some(item) = input.get(index) {
                    let (call_id, name, arguments) = match item {
                        ModelInput::AssistantToolCall {
                            call_id,
                            name,
                            arguments,
                        } => (call_id, name, arguments.clone()),
                        ModelInput::FreeformToolCall {
                            call_id,
                            name,
                            input,
                        } => (call_id, name, json!({ "input": input }).to_string()),
                        _ => break,
                    };
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
                let mut message = json!({ "role": "assistant", "tool_calls": calls });
                if let Some((content, summary)) = pending_reasoning.take() {
                    if let Some(content) = content {
                        message["reasoning_content"] = json!(content);
                    }
                    if let Some(summary) = summary {
                        message["reasoning_summary"] = json!(summary);
                    }
                }
                messages.push(message);
            }
            ModelInput::ToolResult { call_id, output } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output
                }));
                index += 1;
            }
            ModelInput::FreeformToolResult { call_id, output } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output
                }));
                index += 1;
            }
            ModelInput::Reasoning {
                content,
                encrypted_content: _,
                summary,
            } => {
                match &mut pending_reasoning {
                    Some((pending_content, pending_summary)) => {
                        if let Some(content) = content {
                            pending_content
                                .get_or_insert_with(String::new)
                                .push_str(content);
                        }
                        if let Some(summary) = summary {
                            pending_summary
                                .get_or_insert_with(String::new)
                                .push_str(summary);
                        }
                    }
                    None => {
                        pending_reasoning = Some((content.clone(), summary.clone()));
                    }
                }
                index += 1;
            }
        }
    }
    Ok(messages)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCallEncoding {
    FunctionJson,
    WrappedFreeform,
}

/// Incrementally decodes one streamed Chat Completions response.
#[derive(Debug)]
pub struct ChatEventDecoder {
    sse: SseDecoder,
    assistant_text: String,
    usage: Option<ModelUsage>,
    response_id: Option<String>,
    finish_reason: Option<String>,
    tool_call_encodings: BTreeMap<String, ToolCallEncoding>,
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
    /// Creates a decoder without advertised tools.
    pub fn new() -> Self {
        Self::with_tools(DEFAULT_MAX_EVENT_BYTES, &[]).expect("the default event limit is nonzero")
    }

    /// Creates a decoder for the exact tools advertised by one request.
    pub fn with_tools(
        max_event_bytes: usize,
        tools: &[harness_tool_api::ToolDefinition],
    ) -> Result<Self, InvalidEventLimit> {
        let tool_call_encodings = tools
            .iter()
            .map(|tool| {
                let encoding = match &tool.input_schema {
                    ToolInputSchema::FreeformGrammar { .. } => {
                        ToolCallEncoding::WrappedFreeform
                    }
                    ToolInputSchema::JsonSchema(_) => ToolCallEncoding::FunctionJson,
                };
                (tool.name.as_str().to_owned(), encoding)
            })
            .collect();
        Ok(Self {
            sse: SseDecoder::new(max_event_bytes)?,
            assistant_text: String::new(),
            usage: None,
            response_id: None,
            finish_reason: None,
            tool_call_encodings,
            tool_calls: BTreeMap::new(),
            started: false,
            terminal_seen: false,
        })
    }

    /// Feeds bytes and returns every complete typed event.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ModelEvent>, ChatCompletionsProtocolError> {
        let events = self.sse.push(bytes)?;
        self.decode_sse_events(events)
    }

    /// Finishes framing and rejects a stream without the `[DONE]` sentinel.
    pub fn finish(&mut self) -> Result<Vec<ModelEvent>, ChatCompletionsProtocolError> {
        let events = self.sse.finish()?;
        let decoded = self.decode_sse_events(events)?;
        if !self.terminal_seen {
            return Err(ChatCompletionsProtocolError::MissingDone);
        }
        Ok(decoded)
    }

    fn decode_sse_events(
        &mut self,
        events: Vec<harness_responses_protocol::SseEvent>,
    ) -> Result<Vec<ModelEvent>, ChatCompletionsProtocolError> {
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

    fn decode_chunk(
        &mut self,
        payload: &str,
    ) -> Result<Vec<ModelEvent>, ChatCompletionsProtocolError> {
        if self.terminal_seen {
            return Err(ChatCompletionsProtocolError::EventAfterTerminal);
        }
        let value: Value = sonic_rs::from_str(payload)?;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Chat Completions API reported an error");
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
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
            {
                events.push(ModelEvent::ReasoningContentDelta(reasoning.to_owned()));
            }
            if let Some(summary) = delta
                .get("reasoning_summary")
                .or_else(|| delta.get("summary"))
                .and_then(Value::as_str)
            {
                events.push(ModelEvent::ReasoningSummaryDelta(summary.to_owned()));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let index = call.get("index").and_then(Value::as_u64).ok_or(
                    ChatCompletionsProtocolError::InvalidField("tool_calls.index"),
                )?;
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
                            .ok_or(ChatCompletionsProtocolError::InvalidField("tool_calls.id"))?;
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

    fn complete(&mut self) -> Result<Vec<ModelEvent>, ChatCompletionsProtocolError> {
        if self.terminal_seen {
            return Err(ChatCompletionsProtocolError::DuplicateTerminal);
        }
        self.terminal_seen = true;
        let reason = self.finish_reason.as_deref().unwrap_or("stop");
        if !matches!(reason, "stop" | "tool_calls") {
            return Ok(vec![ModelEvent::Terminal(ModelTerminalOutcome::Failed(
                ModelFailure {
                    kind: ModelFailureKind::ProviderRejected,
                    message: format!("Chat Completions API finished with reason {reason:?}"),
                },
            ))]);
        }

        let mut events = Vec::new();
        for (_, partial) in std::mem::take(&mut self.tool_calls) {
            let call_id = partial
                .call_id
                .ok_or(ChatCompletionsProtocolError::InvalidField("tool_calls.id"))?;
            let name = partial
                .name
                .ok_or(ChatCompletionsProtocolError::InvalidField(
                    "tool_calls.function.name",
                ))?;
            let encoding = self.tool_call_encodings.get(&name).ok_or_else(|| {
                ChatCompletionsProtocolError::UnadvertisedToolCall { name: name.clone() }
            })?;
            let input = match encoding {
                ToolCallEncoding::WrappedFreeform => {
                    let wrapped: Value = sonic_rs::from_str(&partial.arguments)
                        .map_err(ChatCompletionsProtocolError::InvalidFreeformArguments)?;
                    let input = wrapped.get("input").and_then(Value::as_str).ok_or(
                        ChatCompletionsProtocolError::InvalidField(
                            "tool_calls.function.arguments.input",
                        ),
                    )?;
                    ToolInput::Freeform(input.to_owned())
                }
                ToolCallEncoding::FunctionJson => ToolInput::FunctionJson(partial.arguments),
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

/// Chat Completions request or stream protocol failure.
#[derive(Debug, Error)]
pub enum ChatCompletionsProtocolError {
    /// SSE framing fails.
    #[error(transparent)]
    Sse(#[from] SseDecodeError),
    /// A response calls a tool that was not advertised by this request.
    #[error(
        "Chat Completions provider returned a call for unadvertised tool `{name}`; valid tool calls must name a tool advertised by this request"
    )]
    UnadvertisedToolCall { name: String },
    /// JSON parsing fails.
    #[error("JSON parsing fails: {0}")]
    Json(#[from] sonic_rs::Error),
    /// A required field is absent or invalid.
    #[error("Chat Completions field `{0}` is missing or invalid")]
    InvalidField(&'static str),
    /// A wrapped freeform call does not contain the exact string input contract.
    #[error("Chat Completions freeform compatibility arguments are not valid JSON: {0}")]
    InvalidFreeformArguments(sonic_rs::Error),
    /// A function tool contains an invalid JSON schema.
    #[error("Chat Completions function tool schema is not valid JSON: {0}")]
    InvalidToolSchema(sonic_rs::Error),
    /// Native freeform input requires the Responses custom-tool format.
    #[error("Chat Completions cannot encode native freeform input")]
    UnsupportedFreeformInput,
    /// The stream closes without the required terminal sentinel.
    #[error("Chat Completions stream ended without [DONE]")]
    MissingDone,
    /// More than one terminal marker is received.
    #[error("Chat Completions stream contains more than one terminal marker")]
    DuplicateTerminal,
    /// Data arrives after a terminal outcome.
    #[error("Chat Completions stream contains data after its terminal outcome")]
    EventAfterTerminal,
}
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use harness_model_api::{
        ModelAttemptId, ModelRequest, ModelRequestId, ModelSelection, ProviderGeneration,
        ProviderId,
    };

    use super::*;

    #[test]
    fn adjacent_reasoning_items_are_preserved_in_one_chat_message() {
        let provider = ProviderId::new("chat").unwrap();
        let attempt = ModelAttempt::initial(
            Arc::new(ModelRequest {
                request_id: ModelRequestId(1),
                context_usage: None,
                provider_generation: ProviderGeneration(1),
                history_revision: 1,
                selection: ModelSelection::new(provider, "model", None, None),
                input: Arc::from([
                    ModelInput::Reasoning {
                        content: Some("raw one ".to_string()),
                        encrypted_content: None,
                        summary: Some("plan one ".to_string()),
                    },
                    ModelInput::Reasoning {
                        content: Some("raw two".to_string()),
                        encrypted_content: None,
                        summary: Some("plan two".to_string()),
                    },
                    ModelInput::Message {
                        role: ModelMessageRole::Assistant,
                        text: "answer".to_string(),
                    },
                ]),
                tools: Arc::from([]),
                previous_response_id: None,
            }),
            ModelAttemptId(1),
        );

        let body = encode_request(&attempt).unwrap();

        assert_eq!(
            body["messages"][0]["reasoning_content"].as_str(),
            Some("raw one raw two")
        );
        assert_eq!(
            body["messages"][0]["reasoning_summary"].as_str(),
            Some("plan one plan two")
        );
        assert_eq!(body["messages"][0]["content"].as_str(), Some("answer"));
    }
}
