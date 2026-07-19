//! Pure incremental framing and typed event decoding for Responses streams.

use harness_model_api::{
    ModelCompletion, ModelEvent, ModelFailure, ModelFailureKind, ModelInterruption, ModelUsage,
    ModelTerminalOutcome, ToolCall, ToolInputDelta,
};
use harness_tool_api::ToolInput;
use serde_json::Value;
use thiserror::Error;

/// Default maximum size of one SSE event payload.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 1_048_576;

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
        })
    }

    /// Feeds bytes and returns every complete event found in the input.
    /// Feeds bytes and returns every complete event found in the input.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, SseDecodeError> {
        let mut events = Vec::new();

        for &byte in bytes {
            self.line.push(byte);
            if self.line.len() > self.max_event_bytes.saturating_add(6) {
                return Err(SseDecodeError::EventTooLarge {
                    limit: self.max_event_bytes,
                });
            }
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.consume_line(&line, &mut events)?;
            }
        }

        Ok(events)
    }


    /// Finishes the stream and rejects an incomplete UTF-8 line or event.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>, SseDecodeError> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.consume_line(&line, &mut events)?;
        }
        if !self.data.is_empty() {
            return Err(SseDecodeError::IncompleteEvent);
        }
        Ok(events)
    }

    fn consume_line(
        &mut self,
        line: &[u8],
        events: &mut Vec<SseEvent>,
    ) -> Result<(), SseDecodeError> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = std::str::from_utf8(line).map_err(SseDecodeError::InvalidUtf8)?;

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

        let Some(value) = line.strip_prefix("data:") else {
            return Err(SseDecodeError::UnexpectedField(line.to_owned()));
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
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
    /// A field other than `data` or a comment appears.
    #[error("unexpected SSE field: {0}")]
    UnexpectedField(String),
    /// An SSE event exceeds the configured byte limit.
    #[error("SSE event exceeds the {limit}-byte limit")]
    EventTooLarge { limit: usize },
    /// A byte sequence is not valid UTF-8.
    #[error("SSE input contains invalid UTF-8")]
    InvalidUtf8(#[source] std::str::Utf8Error),
    /// The stream ends in the middle of an event.
    #[error("SSE stream ends with an incomplete event")]
    IncompleteEvent,
}

/// Decoder state for one Responses model attempt.
#[derive(Debug)]
pub struct ResponsesEventDecoder {
    sse: SseDecoder,
    assistant_text: String,
    usage: Option<ModelUsage>,
    terminal_seen: bool,

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
        events
            .into_iter()
            .map(|event| self.decode_sse_event(&event.data))
            .collect::<Result<Vec<_>, _>>()
            .map(|events| events.into_iter().flatten().collect())
    }

    fn decode_sse_event(&mut self, payload: &str) -> Result<Option<ModelEvent>, ProtocolError> {
        if payload.trim() == "[DONE]" {
            return Ok(None);
        }

        let event = decode_event(payload, &mut self.assistant_text, &mut self.usage)?;
        if matches!(event, ModelEvent::Terminal(_)) {
            if self.terminal_seen {
                return Err(ProtocolError::DuplicateTerminal);
            }
            self.terminal_seen = true;
        }
        Ok(Some(event))
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
}

/// Decodes one complete Responses JSON event.
pub fn decode_event(
    payload: &str,
    assistant_text: &mut String,
    usage: &mut Option<ModelUsage>,
) -> Result<ModelEvent, ProtocolError> {
    let value: Value = serde_json::from_str(payload)?;
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
        "response.function_call_arguments.delta" => Ok(ModelEvent::ToolInputDelta(
            ToolInputDelta {
                call_id: string_field(&value, "item_id")?.to_owned(),
                fragment: string_field(&value, "delta")?.to_owned(),
            },
        )),
        "response.output_item.done" => decode_tool_call(&value),
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
                    .get("response")
                    .and_then(|response| response.get("error"))
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

fn decode_tool_call(value: &Value) -> Result<ModelEvent, ProtocolError> {
    let item = value
        .get("item")
        .ok_or(ProtocolError::InvalidField { field: "item" })?;
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return Err(ProtocolError::UnsupportedEvent(
            item.get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
        ));
    }
    Ok(ModelEvent::ToolCall(ToolCall {
        call_id: string_field(item, "call_id")?.to_owned(),
        name: string_field(item, "name")?.to_owned(),
        input: ToolInput::FunctionJson(string_field(item, "arguments")?.to_owned()),
    }))
}

fn string_field<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, ProtocolError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(ProtocolError::InvalidField { field })
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
        assert!(decoder.push(b"ta: {\"type\":\"response.created\"}\r\n\r\n").unwrap()
            == vec![SseEvent {
                data: "{\"type\":\"response.created\"}".to_owned(),
            }]);
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
    fn event_decoder_keeps_terminal_outcomes_distinct() {
        let mut decoder = ResponsesEventDecoder::new();
        let events = decoder
            .push(b"data: {\"type\":\"response.incomplete\",\"reason\":\"user interrupt\"}\n\n")
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ModelEvent::Terminal(ModelTerminalOutcome::Interrupted(_))]
        ));
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

    #[test]
    fn event_size_limit_is_enforced_before_buffer_growth() {
        let mut decoder = SseDecoder::new(4).unwrap();
        assert!(matches!(
            decoder.push(b"data: five\n"),
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
    }
}
