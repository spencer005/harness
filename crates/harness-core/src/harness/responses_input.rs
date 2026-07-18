//! Pure conversions between session history and Responses API input items.

use std::collections::HashSet;

use serde::Serialize;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::{
    responses::{ResponsesEvent, ResponsesRequestId},
    sessions::{HistoryRecord, MessageRecord, SessionRecord, SessionRecordKind},
    tools::NativeToolRegistry,
};

/// Convert durable history records into Responses API input items.
pub(super) fn history_to_responses_input(
    history: &[HistoryRecord],
    tool_registry: &NativeToolRegistry,
) -> Result<Vec<Value>, sonic_rs::Error> {
    let mut wrapped_freeform_call_ids = HashSet::new();
    history
        .iter()
        .map(|record| {
            history_record_to_input(record, tool_registry, &mut wrapped_freeform_call_ids)
        })
        .collect()
}

/// Build a Responses API message input item for the provided role and text.
pub(super) fn message_input(role: MessageInputRole, text: &str) -> Result<Value, sonic_rs::Error> {
    let content = [MessageContentInput {
        item_type: role.content_type(),
        text,
    }];
    sonic_rs::to_value(&MessageInput {
        item_type: "message",
        role: role.wire_role(),
        content: &content,
    })
}

/// Return the Responses request id carried by an event.
pub(super) fn response_event_request_id(event: &ResponsesEvent) -> Option<ResponsesRequestId> {
    match event {
        ResponsesEvent::RequestStarted(request_id)
        | ResponsesEvent::RequestCompleted(request_id)
        | ResponsesEvent::Frame { request_id, .. }
        | ResponsesEvent::FreeformToolCall { request_id, .. }
        | ResponsesEvent::FreeformToolInputDelta { request_id, .. }
        | ResponsesEvent::FunctionToolCall { request_id, .. }
        | ResponsesEvent::FunctionToolInputDelta { request_id, .. }
        | ResponsesEvent::ServerReasoningIncluded { request_id, .. }
        | ResponsesEvent::ModelsEtag { request_id, .. }
        | ResponsesEvent::ServerModel { request_id, .. } => Some(*request_id),
        ResponsesEvent::Error { request_id, .. } => *request_id,
        ResponsesEvent::Prewarmed | ResponsesEvent::ShutdownComplete => None,
    }
}

/// Extract assistant text deltas from a Responses frame.
pub(super) fn assistant_text_delta_from_frame(frame: &Value) -> Option<&str> {
    let object = frame.as_object()?;
    let frame_type = object.get(&"type").and_then(JsonValueTrait::as_str)?;
    match frame_type {
        "response.output_text.delta" => object.get(&"delta").and_then(JsonValueTrait::as_str),
        _ => None,
    }
}

/// Extract completed assistant text from a Responses frame.
pub(super) fn assistant_text_from_frame(frame: &Value) -> Option<String> {
    let object = frame.as_object()?;
    let frame_type = object.get(&"type").and_then(JsonValueTrait::as_str)?;
    match frame_type {
        "response.output_text.done" => object
            .get(&"text")
            .and_then(JsonValueTrait::as_str)
            .map(ToOwned::to_owned),
        "response.completed" | "response.done" => assistant_text_from_completed_response(frame),
        _ => None,
    }
}

fn assistant_text_from_completed_response(frame: &Value) -> Option<String> {
    let response = frame.as_object()?.get(&"response")?.as_object()?;
    let output = response.get(&"output")?.as_array()?;
    let mut text = String::new();
    for item in output {
        let Some(item_object) = item.as_object() else {
            continue;
        };
        if item_object.get(&"type").and_then(JsonValueTrait::as_str) != Some("message") {
            continue;
        }
        let Some(content) = item_object
            .get(&"content")
            .and_then(|value| value.as_array())
        else {
            continue;
        };
        for part in content {
            let Some(part_object) = part.as_object() else {
                continue;
            };
            let part_type = part_object.get(&"type").and_then(JsonValueTrait::as_str);
            if matches!(part_type, Some("output_text" | "text"))
                && let Some(part_text) = part_object.get(&"text").and_then(JsonValueTrait::as_str)
            {
                text.push_str(part_text);
            }
        }
    }
    (!text.is_empty()).then_some(text)
}

/// Derive a session title from a message.
pub(super) fn title_from_message(message: &str) -> String {
    preview_from_message(message)
}

/// Derive an eighty-character preview from a message.
pub(super) fn preview_from_message(message: &str) -> String {
    let mut preview = message.chars().take(80).collect::<String>();
    if message.chars().count() > 80 {
        preview.push('…');
    }
    preview
}

/// Format Unix milliseconds as a UTC date string.
pub(super) fn utc_date_from_unix_ms(ms: u64) -> String {
    let days = (ms / 1000 / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Rebuild model-visible history from a checkpoint and subsequent session records.
pub(super) fn history_from_session_records(
    checkpoint_history: &[HistoryRecord],
    records: &[SessionRecord],
) -> Vec<HistoryRecord> {
    let mut history = checkpoint_history.to_vec();
    for record in records {
        match &record.kind {
            SessionRecordKind::UserMessage(message) => {
                history.push(HistoryRecord::UserMessage(message.clone()));
            }
            SessionRecordKind::DeveloperMessage(message) => {
                history.push(HistoryRecord::DeveloperMessage(message.clone()));
            }
            SessionRecordKind::AssistantMessage(message) => {
                history.push(HistoryRecord::AssistantMessage(message.clone()));
            }
            SessionRecordKind::FreeformToolCall(call) => {
                history.push(HistoryRecord::FreeformToolCall(call.clone()));
            }
            SessionRecordKind::FreeformToolOutput(output) => {
                history.push(HistoryRecord::FreeformToolOutput(output.clone()));
            }
            SessionRecordKind::FunctionToolCall(call) => {
                history.push(HistoryRecord::FunctionToolCall(call.clone()));
            }
            SessionRecordKind::FunctionToolOutput(output) => {
                history.push(HistoryRecord::FunctionToolOutput(output.clone()));
            }
            SessionRecordKind::SessionMeta(_)
            | SessionRecordKind::TurnContext(_)
            | SessionRecordKind::FreeformToolInputDelta(_)
            | SessionRecordKind::CompactionCheckpoint(_)
            | SessionRecordKind::SessionClosed(_)
            | SessionRecordKind::ProviderSessionBinding(_) => {}
        }
    }
    history
}

fn history_record_to_input(
    record: &HistoryRecord,
    tool_registry: &NativeToolRegistry,
    wrapped_freeform_call_ids: &mut HashSet<String>,
) -> Result<Value, sonic_rs::Error> {
    match record {
        HistoryRecord::UserMessage(message) => message_input(MessageInputRole::User, &message.text),
        HistoryRecord::DeveloperMessage(message) => {
            message_input(MessageInputRole::Developer, &message.text)
        }
        HistoryRecord::AssistantMessage(message) => {
            message_input(MessageInputRole::Assistant, &message.text)
        }
        HistoryRecord::FreeformToolCall(call) => {
            if tool_registry.advertises_function_tool(&call.name) {
                wrapped_freeform_call_ids.insert(call.call_id.clone());
                let arguments =
                    sonic_rs::to_string(&FunctionWrappedFreeformCallInput { input: &call.input })?;
                sonic_rs::to_value(&FunctionToolCallInput {
                    item_type: "function_call",
                    call_id: &call.call_id,
                    name: &call.name,
                    arguments: &arguments,
                    status: "completed",
                })
            } else {
                sonic_rs::to_value(&FreeformToolCallInput {
                    item_type: "custom_tool_call",
                    call_id: &call.call_id,
                    name: &call.name,
                    input: &call.input,
                    status: "completed",
                })
            }
        }
        HistoryRecord::FreeformToolOutput(output) => {
            if wrapped_freeform_call_ids.contains(&output.call_id) {
                sonic_rs::to_value(&FunctionToolOutputInput {
                    item_type: "function_call_output",
                    call_id: &output.call_id,
                    output: &output.output,
                })
            } else {
                sonic_rs::to_value(&FreeformToolOutputInput {
                    item_type: "custom_tool_call_output",
                    call_id: &output.call_id,
                    output: &output.output,
                })
            }
        }
        HistoryRecord::FunctionToolCall(call) => sonic_rs::to_value(&FunctionToolCallInput {
            item_type: "function_call",
            call_id: &call.call_id,
            name: &call.name,
            arguments: &call.arguments,
            status: "completed",
        }),
        HistoryRecord::FunctionToolOutput(output) => sonic_rs::to_value(&FunctionToolOutputInput {
            item_type: "function_call_output",
            call_id: &output.call_id,
            output: &output.output,
        }),
    }
}

/// Message role used while constructing Responses API input items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MessageInputRole {
    /// User role.
    User,
    /// Developer role.
    Developer,
    /// Assistant role.
    Assistant,
}

impl MessageInputRole {
    /// Return the wire role for a Responses API message item.
    pub(super) fn wire_role(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Developer => "developer",
            Self::Assistant => "assistant",
        }
    }

    /// Return the content type for a Responses API message item.
    pub(super) fn content_type(self) -> &'static str {
        match self {
            Self::User => "input_text",
            Self::Developer => "input_text",
            Self::Assistant => "output_text",
        }
    }

    /// Convert this role and message into a durable session record kind.
    pub(super) fn session_record_kind(self, message: MessageRecord) -> SessionRecordKind {
        match self {
            Self::User => SessionRecordKind::UserMessage(message),
            Self::Developer => SessionRecordKind::DeveloperMessage(message),
            Self::Assistant => SessionRecordKind::AssistantMessage(message),
        }
    }

    /// Convert this role and message into a model-visible history record.
    pub(super) fn history_record(self, message: MessageRecord) -> HistoryRecord {
        match self {
            Self::User => HistoryRecord::UserMessage(message),
            Self::Developer => HistoryRecord::DeveloperMessage(message),
            Self::Assistant => HistoryRecord::AssistantMessage(message),
        }
    }

    /// Return the transcript prefix for this message role.
    pub(super) fn transcript_prefix(self) -> &'static str {
        match self {
            Self::User => "user>",
            Self::Developer => "developer>",
            Self::Assistant => "assistant>",
        }
    }
}

#[derive(Debug, Serialize)]
struct MessageInput<'a> {
    #[serde(rename = "type")]
    item_type: &'static str,
    role: &'static str,
    content: &'a [MessageContentInput<'a>],
}

#[derive(Debug, Serialize)]
struct MessageContentInput<'a> {
    #[serde(rename = "type")]
    item_type: &'static str,
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct FreeformToolCallInput<'a> {
    #[serde(rename = "type")]
    item_type: &'static str,
    call_id: &'a str,
    name: &'a str,
    input: &'a str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct FreeformToolOutputInput<'a> {
    #[serde(rename = "type")]
    item_type: &'static str,
    call_id: &'a str,
    output: &'a str,
}

#[derive(Debug, Serialize)]
struct FunctionWrappedFreeformCallInput<'a> {
    input: &'a str,
}

#[derive(Debug, Serialize)]
struct FunctionToolCallInput<'a> {
    #[serde(rename = "type")]
    item_type: &'static str,
    call_id: &'a str,
    name: &'a str,
    arguments: &'a str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct FunctionToolOutputInput<'a> {
    #[serde(rename = "type")]
    item_type: &'static str,
    call_id: &'a str,
    output: &'a str,
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}
