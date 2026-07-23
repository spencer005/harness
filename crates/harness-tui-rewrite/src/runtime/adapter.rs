//! Conversion between `harness-runtime-api` contracts and rewrite-owned domain values.

use harness_runtime_api::{
    Activity, ActivityStatus as RuntimeActivityStatus, AgentStatus as RuntimeAgentStatus,
    ModelSummary, ProviderSummary, RuntimeCommand, RuntimeEvent,
};
use harness_tool_api::ToolInput;

use crate::domain::{
    ActivityState, ActivityStatus, AgentId, AgentState, AgentStatus, ContextUsage, DomainEvent,
    ExternalText, PersistedTranscriptEntry, ProviderKind, ProviderState, ProviderTransport,
    RuntimeRequest, ToolInvocationKind, ToolOutputKind, TranscriptPayload,
};

pub(super) fn adapt_runtime_event(event: RuntimeEvent) -> DomainEvent {
    match event {
        RuntimeEvent::TranscriptAppended(entry) => {
            DomainEvent::AppendTranscript(crate::domain::TranscriptSnapshotEntry {
                sequence: entry.sequence,
                payload: convert_payload(entry.payload),
            })
        }
        RuntimeEvent::TranscriptPageLoaded(page) => {
            let entries = page
                .entries
                .into_iter()
                .map(|e| PersistedTranscriptEntry {
                    sequence: e.sequence.unwrap_or(0),
                    payload: convert_payload(e.payload),
                })
                .collect();
            DomainEvent::TranscriptPage {
                entries,
                next_before_sequence: page.next_before_sequence,
                reached_start: page.reached_start,
            }
        }
        RuntimeEvent::TranscriptCommitted {
            reasoning_sequence,
            assistant_sequence,
        } => DomainEvent::TranscriptCommitted {
            reasoning_sequence,
            assistant_sequence,
        },
        RuntimeEvent::ProviderChanged(summary) => {
            DomainEvent::ProviderChanged(convert_provider(summary))
        }
        RuntimeEvent::ModelChanged(summary) => DomainEvent::ModelChanged(convert_model(summary)),
        RuntimeEvent::AgentChanged(summary) => DomainEvent::AgentUpdated(convert_agent(summary)),
        RuntimeEvent::AgentRemoved(id) => DomainEvent::AgentRemoved(AgentId(id)),
        RuntimeEvent::ActivityChanged(activity) => {
            DomainEvent::ActivityChanged(convert_activity(activity))
        }
        RuntimeEvent::ModelAwaiting(awaiting) => DomainEvent::ModelAwaiting(awaiting),
        RuntimeEvent::ResponseStarted => DomainEvent::ResponseStreamStarted,
        RuntimeEvent::AssistantTextDelta(delta) => {
            DomainEvent::AssistantTextDelta(ExternalText::new(delta))
        }
        RuntimeEvent::ReasoningSummaryDelta(delta) | RuntimeEvent::ReasoningContentDelta(delta) => {
            DomainEvent::ThinkingDelta(ExternalText::new(delta))
        }
        RuntimeEvent::CompactionStarted => DomainEvent::CompactionStarted,
        RuntimeEvent::CompactionCompleted(summary) => {
            DomainEvent::CompactionCompleted(ExternalText::new(summary))
        }
        RuntimeEvent::ResponseFinished(outcome) => match outcome {
            harness_model_api::ModelTerminalOutcome::Completed(_) => {
                DomainEvent::ResponseStreamCompleted
            }
            harness_model_api::ModelTerminalOutcome::Interrupted(_) => {
                DomainEvent::ResponseStreamFailed
            }
            harness_model_api::ModelTerminalOutcome::Cancelled(_) => {
                DomainEvent::ResponseStreamFailed
            }
            harness_model_api::ModelTerminalOutcome::Failed(_) => DomainEvent::ResponseStreamFailed,
        },
        RuntimeEvent::ContextUsage(usage) => DomainEvent::ContextUsage(ContextUsage {
            estimated_input_tokens: usage.estimated_input_tokens,
            max_input_tokens: usage.max_input_tokens,
            compact_at_tokens: usage.compact_at_tokens,
        }),
        RuntimeEvent::AgenticLoopStarted => DomainEvent::AgenticLoopStarted,
        RuntimeEvent::AgenticLoopCompleted => DomainEvent::AgenticLoopCompleted,
        RuntimeEvent::DeveloperModeChanged(enabled) => DomainEvent::DeveloperModeChanged(enabled),
        RuntimeEvent::AssistantFirstToken(ttft_ms) => DomainEvent::AssistantFirstToken(ttft_ms),
        RuntimeEvent::SteeringChanged(queued) => {
            DomainEvent::SteeringChanged(queued.map(ExternalText::new))
        }
        RuntimeEvent::OpenSessionPicker(sessions) => DomainEvent::OpenSessionPicker(
            sessions
                .into_iter()
                .map(|s| crate::picker::SessionMeta {
                    id: s.id,
                    modified: std::time::UNIX_EPOCH + std::time::Duration::from_secs(s.modified_secs),
                    all_text: s.all_text,
                    model: s.model,
                    title: s.title,
                    initial_entries: s.initial_entries,
                })
                .collect(),
        ),
        RuntimeEvent::OpenRewindPicker(options) => DomainEvent::OpenRewindPicker(
            options
                .into_iter()
                .map(|o| crate::picker::RewindOptionMeta {
                    sequence: o.sequence,
                    label: o.label,
                })
                .collect(),
        ),
        RuntimeEvent::Failure(failure) => DomainEvent::Failure(failure.message),
        RuntimeEvent::ShutdownComplete => DomainEvent::ShutdownCompleted,
    }
}

pub(super) fn export_runtime_request(request: RuntimeRequest) -> RuntimeCommand {
    match request {
        RuntimeRequest::SubmitInput { text } => RuntimeCommand::SubmitPrompt { text },
        RuntimeRequest::QueueSteering { text } => RuntimeCommand::QueueSteering { text },
        RuntimeRequest::Retry => RuntimeCommand::Retry,
        RuntimeRequest::SetToolAvailability { pattern, enabled } => {
            RuntimeCommand::SetToolAvailability { pattern, enabled }
        }
        RuntimeRequest::Compact { instruction } => RuntimeCommand::Compact { instruction },
        RuntimeRequest::RetryCompaction { instruction } => {
            RuntimeCommand::RetryCompaction { instruction }
        }
        RuntimeRequest::CancelCompaction => RuntimeCommand::CancelCompaction,
        RuntimeRequest::StopRequestLoop => RuntimeCommand::StopRequestLoop,
        RuntimeRequest::AbortResponse => RuntimeCommand::AbortResponse,
        RuntimeRequest::Interrupt { text } => RuntimeCommand::Interrupt { text },
        RuntimeRequest::LoadTranscriptPage { before_sequence } => {
            RuntimeCommand::LoadOlderTranscript { before_sequence }
        }
        RuntimeRequest::Shutdown => RuntimeCommand::Shutdown,
    }
}

/// Convert a transcript snapshot entry from the runtime API format to TUI domain format.
pub fn convert_snapshot_entry(
    entry: harness_runtime_api::TranscriptSnapshotEntry,
) -> crate::domain::TranscriptSnapshotEntry {
    crate::domain::TranscriptSnapshotEntry {
        sequence: entry.sequence,
        payload: convert_payload(entry.payload),
    }
}

/// Convert a transcript payload from the runtime API format to TUI domain format.
pub fn convert_payload(payload: harness_runtime_api::TranscriptPayload) -> TranscriptPayload {
    match payload {
        harness_runtime_api::TranscriptPayload::Message { role, text } => {
            TranscriptPayload::Message {
                role,
                text: ExternalText::new(text),
            }
        }
        harness_runtime_api::TranscriptPayload::ToolCall {
            call_id,
            name,
            input,
        } => {
            let kind = match input {
                ToolInput::Freeform(_) => ToolInvocationKind::Freeform,
                ToolInput::FunctionJson(_) => ToolInvocationKind::Function,
            };
            TranscriptPayload::ToolCall {
                call_id: ExternalText::new(call_id),
                name: ExternalText::new(name),
                input: ExternalText::new(input.as_str()),
                kind,
            }
        }
        harness_runtime_api::TranscriptPayload::ToolResult { call_id, output } => {
            TranscriptPayload::ToolOutput {
                call_id: ExternalText::new(call_id),
                output: ExternalText::new(output),
                display_output: None,
                kind: ToolOutputKind::Function,
            }
        }
        harness_runtime_api::TranscriptPayload::PlainText(text) => {
            TranscriptPayload::PlainText(ExternalText::new(text))
        }
        harness_runtime_api::TranscriptPayload::Thinking { text } => {
            TranscriptPayload::Thinking(ExternalText::new(text))
        }
        harness_runtime_api::TranscriptPayload::Error { category, message } => {
            TranscriptPayload::Error {
                category,
                message: ExternalText::new(message),
            }
        }
    }
}

fn convert_provider(summary: ProviderSummary) -> ProviderState {
    ProviderState {
        display_name: ExternalText::new(summary.display_name),
        kind: derive_provider_kind(&summary.provider),
        transport: derive_transport(&summary.transport),
    }
}

fn derive_provider_kind(provider_id: &str) -> ProviderKind {
    match provider_id {
        "codex" => ProviderKind::Codex,
        "ollama-cloud" => ProviderKind::OllamaCloud,
        _ => ProviderKind::HttpsApi,
    }
}

fn derive_transport(transport: &str) -> ProviderTransport {
    match transport.to_lowercase().as_str() {
        "ws" | "websocket" => ProviderTransport::WebSocket,
        _ => ProviderTransport::Https,
    }
}

fn convert_model(summary: ModelSummary) -> crate::domain::ModelState {
    crate::domain::ModelState {
        model: ExternalText::new(summary.selection.model),
        reasoning_effort: summary.selection.reasoning_effort.map(ExternalText::new),
        service_tier: summary.selection.service_tier.map(ExternalText::new),
    }
}

fn convert_agent(summary: harness_runtime_api::AgentSummary) -> AgentState {
    AgentState {
        id: AgentId(summary.agent_id),
        path: ExternalText::new(summary.path),
        status: match summary.status {
            RuntimeAgentStatus::Running => AgentStatus::Running,
            RuntimeAgentStatus::Waiting => AgentStatus::Waiting,
            RuntimeAgentStatus::Completed(message) => {
                AgentStatus::Completed(ExternalText::new(message))
            }
            RuntimeAgentStatus::Failed(message) => AgentStatus::Failed(ExternalText::new(message)),
            RuntimeAgentStatus::Interrupted => AgentStatus::Interrupted,
        },
        last_task_message: summary.last_task_message.map(ExternalText::new),
        last_activity_message: summary.last_activity_message.map(ExternalText::new),
    }
}

fn convert_activity(activity: Activity) -> ActivityState {
    ActivityState {
        id: ExternalText::new(activity.activity_id),
        description: ExternalText::new(activity.description),
        status: match activity.status {
            RuntimeActivityStatus::Running => ActivityStatus::Running,
            RuntimeActivityStatus::Completed => ActivityStatus::Completed,
            RuntimeActivityStatus::Failed => ActivityStatus::Failed,
        },
        detail: activity.detail.map(ExternalText::new),
    }
}

#[cfg(test)]
mod tests {
    use harness_runtime_api::{
        Activity, ActivityStatus as RuntimeActivityStatus, ContextUsage as RuntimeContextUsage,
        ModelSummary, ProviderSummary, RuntimeCommand, RuntimeEvent, TranscriptPage,
        TranscriptSnapshotEntry,
    };
    use harness_tool_api::ToolInput;

    use super::{adapt_runtime_event, export_runtime_request};
    use crate::domain::{
        ActivityStatus, DomainEvent, ExternalText, RuntimeRequest, TranscriptPayload,
    };

    #[test]
    fn transcript_appended_directly_produces_append_event() {
        let event =
            adapt_runtime_event(RuntimeEvent::TranscriptAppended(TranscriptSnapshotEntry {
                sequence: Some(42),
                payload: harness_runtime_api::TranscriptPayload::Message {
                    role: harness_runtime_api::MessageRole::Assistant,
                    text: "hello".into(),
                },
            }));

        let DomainEvent::AppendTranscript(entry) = event else {
            panic!("transcript appended adapts to append event");
        };
        assert_eq!(entry.sequence, Some(42));
        assert!(matches!(
            entry.payload,
            TranscriptPayload::Message { text, .. } if text.as_str() == "hello"
        ));
    }

    #[test]
    fn transcript_page_preserves_entries_and_sequence_boundaries() {
        let page = TranscriptPage {
            entries: vec![
                TranscriptSnapshotEntry {
                    sequence: Some(1),
                    payload: harness_runtime_api::TranscriptPayload::PlainText("skip".into()),
                },
                TranscriptSnapshotEntry {
                    sequence: Some(2),
                    payload: harness_runtime_api::TranscriptPayload::Message {
                        role: harness_runtime_api::MessageRole::User,
                        text: "visible".into(),
                    },
                },
            ],
            next_before_sequence: Some(0),
            reached_start: false,
        };

        let DomainEvent::TranscriptPage {
            entries,
            next_before_sequence,
            reached_start,
        } = adapt_runtime_event(RuntimeEvent::TranscriptPageLoaded(page))
        else {
            panic!("transcript page adapts to a domain page");
        };

        assert_eq!(next_before_sequence, Some(0));
        assert!(!reached_start);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].sequence, 2);
        assert!(matches!(
            &entries[1].payload,
            TranscriptPayload::Message { text, .. } if text.as_str() == "visible"
        ));
    }

    #[test]
    fn tool_call_converts_input_encoding_to_kind() {
        let freeform =
            adapt_runtime_event(RuntimeEvent::TranscriptAppended(TranscriptSnapshotEntry {
                sequence: None,
                payload: harness_runtime_api::TranscriptPayload::ToolCall {
                    call_id: "call-1".into(),
                    name: "echo".into(),
                    input: ToolInput::Freeform("raw text".into()),
                },
            }));
        let DomainEvent::AppendTranscript(entry) = freeform else {
            panic!("freeform tool call adapts");
        };
        let TranscriptPayload::ToolCall { kind, input, .. } = entry.payload else {
            panic!("freeform tool call payload adapts");
        };
        assert_eq!(kind, super::ToolInvocationKind::Freeform);
        assert_eq!(input.as_str(), "raw text");

        let function =
            adapt_runtime_event(RuntimeEvent::TranscriptAppended(TranscriptSnapshotEntry {
                sequence: None,
                payload: harness_runtime_api::TranscriptPayload::ToolCall {
                    call_id: "call-2".into(),
                    name: "run".into(),
                    input: ToolInput::FunctionJson("{\"cmd\":true}".into()),
                },
            }));
        let DomainEvent::AppendTranscript(entry) = function else {
            panic!("function tool call adapts");
        };
        let TranscriptPayload::ToolCall { kind, input, .. } = entry.payload else {
            panic!("function tool call payload adapts");
        };
        assert_eq!(kind, super::ToolInvocationKind::Function);
        assert_eq!(input.as_str(), "{\"cmd\":true}");
    }

    #[test]
    fn provider_changed_maps_display_name_and_derives_kind() {
        let event = adapt_runtime_event(RuntimeEvent::ProviderChanged(ProviderSummary {
            provider: "codex".into(),
            display_name: "Codex".into(),
            transport: "ws".into(),
        }));

        let DomainEvent::ProviderChanged(provider) = event else {
            panic!("provider changed adapts");
        };
        assert_eq!(provider.display_name.as_str(), "Codex");
        assert_eq!(provider.kind, crate::domain::ProviderKind::Codex);
        assert_eq!(
            provider.transport,
            crate::domain::ProviderTransport::WebSocket
        );
    }

    #[test]
    fn model_changed_extracts_selection_fields() {
        let event = adapt_runtime_event(RuntimeEvent::ModelChanged(ModelSummary {
            selection: harness_model_api::ModelSelection::new(
                harness_model_api::ProviderId::new("test").unwrap(),
                "gpt-test",
                Some("high".into()),
                None,
            ),
            capabilities: Default::default(),
            context_limits: harness_model_api::ContextLimits::new(100_000, 80_000).unwrap(),
        }));

        let DomainEvent::ModelChanged(model) = event else {
            panic!("model changed adapts");
        };
        assert_eq!(model.model.as_str(), "gpt-test");
        assert_eq!(
            model.reasoning_effort.as_ref().map(ExternalText::as_str),
            Some("high")
        );
        assert_eq!(model.service_tier, None);
    }

    #[test]
    fn activity_changed_translates_typed_status_without_validation() {
        let event = adapt_runtime_event(RuntimeEvent::ActivityChanged(Activity {
            activity_id: "worker-1".into(),
            description: "worker".into(),
            status: RuntimeActivityStatus::Completed,
            detail: Some("done".into()),
        }));

        let DomainEvent::ActivityChanged(activity) = event else {
            panic!("activity changed adapts");
        };
        assert_eq!(activity.status, ActivityStatus::Completed);
        assert_eq!(
            activity.detail.as_ref().map(ExternalText::as_str),
            Some("done")
        );
    }

    #[test]
    fn context_usage_carries_threshold_values() {
        let event = adapt_runtime_event(RuntimeEvent::ContextUsage(RuntimeContextUsage {
            estimated_input_tokens: 75_000,
            max_input_tokens: 100_000,
            compact_at_tokens: 80_000,
        }));

        let DomainEvent::ContextUsage(usage) = event else {
            panic!("context usage adapts");
        };
        assert_eq!(usage.estimated_input_tokens, 75_000);
        assert_eq!(usage.compact_at_tokens, 80_000);
    }

    #[test]
    fn runtime_requests_export_with_clean_boundary_names() {
        let RuntimeCommand::Interrupt { text } =
            export_runtime_request(RuntimeRequest::Interrupt {
                text: "\r\n\u{1b}exact".into(),
            })
        else {
            panic!("immediate steering exports to Interrupt");
        };
        assert_eq!(text, "\r\n\u{1b}exact");

        let RuntimeCommand::LoadOlderTranscript { before_sequence } =
            export_runtime_request(RuntimeRequest::LoadTranscriptPage {
                before_sequence: Some(73),
            })
        else {
            panic!("page request exports to LoadOlderTranscript");
        };
        assert_eq!(before_sequence, Some(73));

        assert_eq!(
            export_runtime_request(RuntimeRequest::Retry),
            RuntimeCommand::Retry
        );
    }

    #[test]
    fn failure_event_surfaces_message_to_domain() {
        let event =
            adapt_runtime_event(RuntimeEvent::Failure(harness_runtime_api::RuntimeFailure {
                category: harness_runtime_api::RuntimeFailureCategory::Model,
                message: "model unreachable".into(),
            }));
        let DomainEvent::Failure(message) = event else {
            panic!("failure adapts to failure");
        };
        assert_eq!(message, "model unreachable");
    }
}
