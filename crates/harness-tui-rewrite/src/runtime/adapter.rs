//! Exhaustive conversion between `harness-core` DTOs and rewrite-owned domain values.

use harness_core::{
    UiSnapshot, UiTranscriptEntry,
    actors::{RuntimeCommand, RuntimeEvent},
    compact::ContextWindowUsage,
    providers::{ProviderKind as CoreProviderKind, ProviderTransportKind, ProviderUiInfo},
    responses::ModelSettings,
    sessions::{
        FreeformToolCallRecord, FreeformToolOutputRecord, FunctionToolCallRecord,
        FunctionToolOutputRecord, InspectReadDisplayRecord, InspectReadNextRecord,
        SessionRecordKind, ToolOutputDisplayRecord, TranscriptPage,
    },
    steering::SteeringMode,
    subagents::{AgentId as CoreAgentId, AgentStatus as CoreAgentStatus, AgentSummary},
};

use crate::domain::{
    ActivityState, ActivityStatus, AgentId, AgentState, AgentStatus, ContextUsage, DomainEvent,
    ExternalText, FinalState, InitialState, InspectReadDisplay, InspectReadNext, MessageRole,
    ModelState, PersistedTranscriptEntry, ProviderKind, ProviderState, ProviderTransport,
    RuntimeRequest, ToolInvocationKind, ToolOutputDisplay, ToolOutputKind, TranscriptPayload,
    TranscriptSnapshotEntry,
};

pub(super) fn import_snapshot(snapshot: UiSnapshot) -> InitialState {
    InitialState {
        session_id: ExternalText::new(snapshot.session_id),
        thread_title: ExternalText::new(snapshot.thread_title),
        provider: snapshot.provider.map(import_provider),
        model: import_model(snapshot.model_settings),
        developer_mode: snapshot.developer_mode,
        response_streaming: snapshot.response_streaming,
        last_ttft_ms: snapshot.last_ttft_ms,
        transcript: snapshot
            .transcript_entries
            .into_iter()
            .filter_map(import_ui_transcript_entry)
            .collect(),
        prompt: snapshot.input,
        prompt_cursor: snapshot.input_cursor,
        queued_steering: snapshot.queued_steering_prompt.map(ExternalText::new),
        agents: snapshot.agents.into_iter().map(import_agent).collect(),
        active_activity_ids: snapshot
            .active_activities
            .into_iter()
            .map(ExternalText::new)
            .collect(),
    }
}

pub(super) fn export_snapshot(state: FinalState) -> UiSnapshot {
    UiSnapshot {
        session_id: state.session_id.into_string(),
        thread_title: state.thread_title.into_string(),
        provider: state.provider.map(export_provider),
        model_settings: export_model(state.model),
        developer_mode: state.developer_mode,
        response_streaming: state.response_streaming,
        last_ttft_ms: state.last_ttft_ms,
        transcript_entries: state
            .transcript
            .into_iter()
            .map(export_transcript_entry)
            .collect(),
        input: state.prompt,
        input_cursor: state.prompt_cursor,
        queued_steering_prompt: state.queued_steering.map(ExternalText::into_string),
        agents: state.agents.into_iter().map(export_agent).collect(),
        active_activities: state
            .active_activity_ids
            .into_iter()
            .map(ExternalText::into_string)
            .collect(),
    }
}

pub(super) fn adapt_runtime_event(event: RuntimeEvent) -> DomainEvent {
    match event {
        RuntimeEvent::TranscriptLine(text) => {
            DomainEvent::AppendTranscript(TranscriptPayload::PlainText(ExternalText::new(text)))
        }
        RuntimeEvent::FreeformToolCall(record) => {
            DomainEvent::AppendTranscript(import_freeform_tool_call(record))
        }
        RuntimeEvent::FreeformToolOutput(record) => {
            DomainEvent::AppendTranscript(import_freeform_tool_output(record))
        }
        RuntimeEvent::FunctionToolCall(record) => {
            DomainEvent::AppendTranscript(import_function_tool_call(record))
        }
        RuntimeEvent::FunctionToolOutput(record) => {
            DomainEvent::AppendTranscript(import_function_tool_output(record))
        }
        RuntimeEvent::TranscriptPage(page) => import_transcript_page(page),
        RuntimeEvent::ModelSettingsChanged(settings) => {
            DomainEvent::ModelChanged(import_model(settings))
        }
        RuntimeEvent::ProviderChanged(provider) => {
            DomainEvent::ProviderChanged(import_provider(provider))
        }
        RuntimeEvent::ContextWindowUsage(usage) => {
            DomainEvent::ContextUsage(import_context_usage(usage))
        }
        RuntimeEvent::AgenticLoopStarted => DomainEvent::AgenticLoopStarted,
        RuntimeEvent::AgenticLoopCompleted => DomainEvent::AgenticLoopCompleted,
        RuntimeEvent::DeveloperModeChanged(enabled) => DomainEvent::DeveloperModeChanged(enabled),
        RuntimeEvent::ResponseStreamStarted => DomainEvent::ResponseStreamStarted,
        RuntimeEvent::AssistantFirstToken { ttft_ms } => DomainEvent::AssistantFirstToken(ttft_ms),
        RuntimeEvent::AssistantTextDelta(delta) => {
            DomainEvent::AssistantTextDelta(ExternalText::new(delta))
        }
        RuntimeEvent::ResponseStreamCompleted => DomainEvent::ResponseStreamCompleted,
        RuntimeEvent::Responses(_) => DomainEvent::LowLevelResponseObserved,
        RuntimeEvent::AgentUpdated(agent) => DomainEvent::AgentUpdated(import_agent(agent)),
        RuntimeEvent::AgentRemoved(agent) => DomainEvent::AgentRemoved(AgentId(agent.0)),
        RuntimeEvent::CompactCompleted(result) => {
            DomainEvent::CompactionCompleted(ExternalText::new(result.summary))
        }
        RuntimeEvent::SteeringQueued(queued) => {
            DomainEvent::SteeringChanged(queued.map(ExternalText::new))
        }
        RuntimeEvent::AgentMailboxUpdate { agent_id } => {
            DomainEvent::AgentMailboxChanged(AgentId(agent_id.0))
        }
        RuntimeEvent::SubagentActivity {
            activity_id,
            description,
            status,
            detail,
        } => {
            let status = match status.as_str() {
                "running" => ActivityStatus::Running,
                "completed" => ActivityStatus::Completed,
                "failed" => ActivityStatus::Failed,
                _ => {
                    return DomainEvent::ProtocolViolation(ExternalText::new(format!(
                        "runtime activity {activity_id} has unknown status {status:?}"
                    )));
                }
            };
            DomainEvent::ActivityChanged(ActivityState {
                id: ExternalText::new(activity_id),
                description: ExternalText::new(description),
                status,
                detail: detail.map(ExternalText::new),
            })
        }
        RuntimeEvent::ShutdownComplete => DomainEvent::ShutdownCompleted,
    }
}

/// Entry budget required by the current core paging API.
///
/// `RuntimeCommand::LoadTranscriptPage::max_lines` is misnamed: session storage
/// counts persisted displayable entries, not rendered lines. Page sizing is not
/// part of the rewrite domain; this adapter quarantines the current core
/// contract until the runtime owns its storage-page policy.
const CORE_TRANSCRIPT_PAGE_ENTRY_LIMIT: usize = 96;

pub(super) fn export_runtime_request(request: RuntimeRequest) -> RuntimeCommand {
    match request {
        RuntimeRequest::SubmitInput { text } => RuntimeCommand::SubmitInput { text },
        RuntimeRequest::QueueSteering { text } => RuntimeCommand::QueueSteering { text },
        RuntimeRequest::ApplySteering { text } => RuntimeCommand::ApplySteering {
            text,
            mode: SteeringMode::InterruptNow,
        },
        RuntimeRequest::LoadTranscriptPage { before_sequence } => {
            RuntimeCommand::LoadTranscriptPage {
                before_seq: before_sequence,
                max_lines: CORE_TRANSCRIPT_PAGE_ENTRY_LIMIT,
            }
        }
        RuntimeRequest::Shutdown => RuntimeCommand::Shutdown,
    }
}

fn import_ui_transcript_entry(entry: UiTranscriptEntry) -> Option<TranscriptSnapshotEntry> {
    let (sequence, payload) = match entry {
        UiTranscriptEntry::Text {
            source_sequence,
            text,
        } => (
            source_sequence,
            TranscriptPayload::PlainText(ExternalText::new(text)),
        ),
        UiTranscriptEntry::SessionRecord {
            source_sequence,
            record,
        } => (source_sequence, import_session_record(record)?),
    };
    Some(TranscriptSnapshotEntry { sequence, payload })
}

fn import_transcript_page(page: TranscriptPage) -> DomainEvent {
    let entries = page
        .lines
        .into_iter()
        .filter_map(|line| {
            import_session_record(line.kind).map(|payload| PersistedTranscriptEntry {
                sequence: line.seq,
                payload,
            })
        })
        .collect();
    DomainEvent::TranscriptPage {
        entries,
        next_before_sequence: page.next_before_seq,
        reached_start: page.reached_start,
    }
}

fn import_session_record(record: SessionRecordKind) -> Option<TranscriptPayload> {
    match record {
        SessionRecordKind::UserMessage(record) => Some(TranscriptPayload::Message {
            role: MessageRole::User,
            text: ExternalText::new(record.text),
        }),
        SessionRecordKind::DeveloperMessage(record) => Some(TranscriptPayload::Message {
            role: MessageRole::Developer,
            text: ExternalText::new(record.text),
        }),
        SessionRecordKind::AssistantMessage(record) => Some(TranscriptPayload::Message {
            role: MessageRole::Assistant,
            text: ExternalText::new(record.text),
        }),
        SessionRecordKind::FreeformToolCall(record) => Some(import_freeform_tool_call(record)),
        SessionRecordKind::FreeformToolOutput(record) => Some(import_freeform_tool_output(record)),
        SessionRecordKind::FunctionToolCall(record) => Some(import_function_tool_call(record)),
        SessionRecordKind::FunctionToolOutput(record) => Some(import_function_tool_output(record)),
        SessionRecordKind::SessionClosed(record) => Some(TranscriptPayload::SessionClosed {
            closed_at_ms: record.closed_at_ms,
        }),
        SessionRecordKind::SessionMeta(_)
        | SessionRecordKind::TurnContext(_)
        | SessionRecordKind::FreeformToolInputDelta(_)
        | SessionRecordKind::CompactionCheckpoint(_)
        | SessionRecordKind::ProviderSessionBinding(_) => None,
    }
}

fn export_transcript_entry(entry: TranscriptSnapshotEntry) -> UiTranscriptEntry {
    use harness_core::sessions::{MessageRecord, SessionClosedRecord};

    let TranscriptSnapshotEntry { sequence, payload } = entry;
    match payload {
        TranscriptPayload::Message { role, text } => {
            let record = MessageRecord {
                text: text.into_string(),
            };
            UiTranscriptEntry::SessionRecord {
                source_sequence: sequence,
                record: match role {
                    MessageRole::User => SessionRecordKind::UserMessage(record),
                    MessageRole::Developer => SessionRecordKind::DeveloperMessage(record),
                    MessageRole::Assistant => SessionRecordKind::AssistantMessage(record),
                },
            }
        }
        TranscriptPayload::PlainText(text) | TranscriptPayload::Event(text) => {
            UiTranscriptEntry::Text {
                source_sequence: sequence,
                text: text.into_string(),
            }
        }
        TranscriptPayload::ToolCall {
            call_id,
            name,
            input,
            kind,
        } => UiTranscriptEntry::SessionRecord {
            source_sequence: sequence,
            record: match kind {
                ToolInvocationKind::Freeform => {
                    SessionRecordKind::FreeformToolCall(FreeformToolCallRecord {
                        call_id: call_id.into_string(),
                        name: name.into_string(),
                        input: input.into_string(),
                    })
                }
                ToolInvocationKind::Function => {
                    SessionRecordKind::FunctionToolCall(FunctionToolCallRecord {
                        call_id: call_id.into_string(),
                        name: name.into_string(),
                        arguments: input.into_string(),
                    })
                }
            },
        },
        TranscriptPayload::ToolOutput {
            call_id,
            output,
            display_output,
            kind,
        } => UiTranscriptEntry::SessionRecord {
            source_sequence: sequence,
            record: match kind {
                ToolOutputKind::Freeform { display } => {
                    SessionRecordKind::FreeformToolOutput(FreeformToolOutputRecord {
                        call_id: call_id.into_string(),
                        output: output.into_string(),
                        display_output: display_output.map(ExternalText::into_string),
                        display: display.map(export_tool_output_display),
                    })
                }
                ToolOutputKind::Function => {
                    SessionRecordKind::FunctionToolOutput(FunctionToolOutputRecord {
                        call_id: call_id.into_string(),
                        output: output.into_string(),
                        display_output: display_output.map(ExternalText::into_string),
                    })
                }
            },
        },
        TranscriptPayload::SessionClosed { closed_at_ms } => UiTranscriptEntry::SessionRecord {
            source_sequence: sequence,
            record: SessionRecordKind::SessionClosed(SessionClosedRecord { closed_at_ms }),
        },
    }
}

fn import_freeform_tool_call(record: FreeformToolCallRecord) -> TranscriptPayload {
    TranscriptPayload::ToolCall {
        call_id: ExternalText::new(record.call_id),
        name: ExternalText::new(record.name),
        input: ExternalText::new(record.input),
        kind: ToolInvocationKind::Freeform,
    }
}

fn import_function_tool_call(record: FunctionToolCallRecord) -> TranscriptPayload {
    TranscriptPayload::ToolCall {
        call_id: ExternalText::new(record.call_id),
        name: ExternalText::new(record.name),
        input: ExternalText::new(record.arguments),
        kind: ToolInvocationKind::Function,
    }
}

fn import_freeform_tool_output(record: FreeformToolOutputRecord) -> TranscriptPayload {
    TranscriptPayload::ToolOutput {
        call_id: ExternalText::new(record.call_id),
        output: ExternalText::new(record.output),
        display_output: record.display_output.map(ExternalText::new),
        kind: ToolOutputKind::Freeform {
            display: record.display.map(import_tool_output_display),
        },
    }
}

fn import_function_tool_output(record: FunctionToolOutputRecord) -> TranscriptPayload {
    TranscriptPayload::ToolOutput {
        call_id: ExternalText::new(record.call_id),
        output: ExternalText::new(record.output),
        display_output: record.display_output.map(ExternalText::new),
        kind: ToolOutputKind::Function,
    }
}

fn import_tool_output_display(display: ToolOutputDisplayRecord) -> ToolOutputDisplay {
    match display {
        ToolOutputDisplayRecord::InspectRead(reads) => {
            ToolOutputDisplay::InspectRead(reads.into_iter().map(import_inspect_read).collect())
        }
    }
}

fn export_tool_output_display(display: ToolOutputDisplay) -> ToolOutputDisplayRecord {
    match display {
        ToolOutputDisplay::InspectRead(reads) => ToolOutputDisplayRecord::InspectRead(
            reads.into_iter().map(export_inspect_read).collect(),
        ),
    }
}

fn import_inspect_read(read: InspectReadDisplayRecord) -> InspectReadDisplay {
    InspectReadDisplay {
        path: ExternalText::new(read.path),
        start_line: read.start_line,
        lines: read.lines.into_iter().map(ExternalText::new).collect(),
        next: read.next.map(|next| InspectReadNext {
            start_line: next.start_line,
            line_count: next.line_count,
        }),
    }
}

fn export_inspect_read(read: InspectReadDisplay) -> InspectReadDisplayRecord {
    InspectReadDisplayRecord {
        path: read.path.into_string(),
        start_line: read.start_line,
        lines: read
            .lines
            .into_iter()
            .map(ExternalText::into_string)
            .collect(),
        next: read.next.map(|next| InspectReadNextRecord {
            start_line: next.start_line,
            line_count: next.line_count,
        }),
    }
}

fn import_provider(provider: ProviderUiInfo) -> ProviderState {
    ProviderState {
        display_name: ExternalText::new(provider.display_name),
        kind: match provider.kind {
            CoreProviderKind::Codex => ProviderKind::Codex,
            CoreProviderKind::OllamaCloud => ProviderKind::OllamaCloud,
            CoreProviderKind::HttpsApi => ProviderKind::HttpsApi,
        },
        transport: match provider.transport {
            ProviderTransportKind::WebSocket => ProviderTransport::WebSocket,
            ProviderTransportKind::Https => ProviderTransport::Https,
        },
    }
}

fn export_provider(provider: ProviderState) -> ProviderUiInfo {
    ProviderUiInfo {
        display_name: provider.display_name.into_string(),
        kind: match provider.kind {
            ProviderKind::Codex => CoreProviderKind::Codex,
            ProviderKind::OllamaCloud => CoreProviderKind::OllamaCloud,
            ProviderKind::HttpsApi => CoreProviderKind::HttpsApi,
        },
        transport: match provider.transport {
            ProviderTransport::WebSocket => ProviderTransportKind::WebSocket,
            ProviderTransport::Https => ProviderTransportKind::Https,
        },
    }
}

fn import_model(settings: ModelSettings) -> ModelState {
    ModelState {
        model: ExternalText::new(settings.model),
        reasoning_effort: settings.reasoning_effort.map(ExternalText::new),
        service_tier: settings.service_tier.map(ExternalText::new),
    }
}

fn export_model(model: ModelState) -> ModelSettings {
    ModelSettings {
        model: model.model.into_string(),
        reasoning_effort: model.reasoning_effort.map(ExternalText::into_string),
        service_tier: model.service_tier.map(ExternalText::into_string),
    }
}

fn import_context_usage(usage: ContextWindowUsage) -> ContextUsage {
    ContextUsage {
        estimated_input_tokens: usage.estimated_input_tokens,
        max_input_tokens: usage.max_input_tokens,
        compact_at_tokens: usage.compact_at_tokens,
    }
}

fn import_agent(agent: AgentSummary) -> AgentState {
    AgentState {
        id: AgentId(agent.id.0),
        path: ExternalText::new(agent.path),
        status: match agent.status {
            CoreAgentStatus::Running => AgentStatus::Running,
            CoreAgentStatus::Waiting => AgentStatus::Waiting,
            CoreAgentStatus::Completed(message) => {
                AgentStatus::Completed(ExternalText::new(message))
            }
            CoreAgentStatus::Failed(message) => AgentStatus::Failed(ExternalText::new(message)),
            CoreAgentStatus::Interrupted => AgentStatus::Interrupted,
        },
        last_task_message: agent.last_task_message.map(ExternalText::new),
        last_activity_message: agent.last_activity_message.map(ExternalText::new),
    }
}

fn export_agent(agent: AgentState) -> AgentSummary {
    AgentSummary {
        id: CoreAgentId(agent.id.0),
        path: agent.path.into_string(),
        status: match agent.status {
            AgentStatus::Running => CoreAgentStatus::Running,
            AgentStatus::Waiting => CoreAgentStatus::Waiting,
            AgentStatus::Completed(message) => CoreAgentStatus::Completed(message.into_string()),
            AgentStatus::Failed(message) => CoreAgentStatus::Failed(message.into_string()),
            AgentStatus::Interrupted => CoreAgentStatus::Interrupted,
        },
        last_task_message: agent.last_task_message.map(ExternalText::into_string),
        last_activity_message: agent.last_activity_message.map(ExternalText::into_string),
    }
}

#[cfg(test)]
mod tests {
    use harness_core::{
        UiSnapshot, UiTranscriptEntry,
        actors::{RuntimeCommand, RuntimeEvent},
        providers::{ProviderKind, ProviderTransportKind, ProviderUiInfo},
        responses::ModelSettings,
        sessions::{
            FreeformToolCallRecord, FreeformToolInputDeltaRecord, FreeformToolOutputRecord,
            FunctionToolCallRecord, FunctionToolOutputRecord, InspectReadDisplayRecord,
            InspectReadNextRecord, MessageRecord, SessionClosedRecord, SessionRecordKind,
            ToolOutputDisplayRecord, TranscriptPage, TranscriptPageLine, TurnContextRecord,
        },
        steering::SteeringMode,
        subagents::{AgentId, AgentStatus, AgentSummary},
    };

    use super::{
        CORE_TRANSCRIPT_PAGE_ENTRY_LIMIT, adapt_runtime_event, export_runtime_request,
        export_snapshot, import_snapshot,
    };
    use crate::{
        app::Application,
        domain::{ActivityStatus, DomainEvent, RuntimeRequest, TranscriptPayload},
    };

    fn snapshot_with_model_settings(model_settings: ModelSettings) -> UiSnapshot {
        UiSnapshot {
            session_id: "session".to_string(),
            thread_title: "thread".to_string(),
            provider: None,
            model_settings,
            developer_mode: false,
            response_streaming: false,
            last_ttft_ms: None,
            transcript_entries: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            queued_steering_prompt: None,
            agents: Vec::new(),
            active_activities: Vec::new(),
        }
    }

    #[test]
    fn snapshot_round_trip_preserves_frontend_owned_values() {
        let transcript_entries = vec![
            UiTranscriptEntry::Text {
                source_sequence: Some(1),
                text: "runtime \u{1b}[31mtext".to_string(),
            },
            UiTranscriptEntry::SessionRecord {
                source_sequence: Some(2),
                record: SessionRecordKind::UserMessage(MessageRecord {
                    text: "user".to_string(),
                }),
            },
            UiTranscriptEntry::SessionRecord {
                source_sequence: Some(3),
                record: SessionRecordKind::DeveloperMessage(MessageRecord {
                    text: "developer".to_string(),
                }),
            },
            UiTranscriptEntry::SessionRecord {
                source_sequence: Some(4),
                record: SessionRecordKind::AssistantMessage(MessageRecord {
                    text: "assistant".to_string(),
                }),
            },
            UiTranscriptEntry::SessionRecord {
                source_sequence: Some(5),
                record: SessionRecordKind::FreeformToolCall(FreeformToolCallRecord {
                    call_id: "free-call".to_string(),
                    name: "inspect".to_string(),
                    input: "read src/lib.rs 1-2".to_string(),
                }),
            },
            UiTranscriptEntry::SessionRecord {
                source_sequence: Some(6),
                record: SessionRecordKind::FreeformToolOutput(FreeformToolOutputRecord {
                    call_id: "free-call".to_string(),
                    output: "model output".to_string(),
                    display_output: Some("display \u{1b}[32moutput".to_string()),
                    display: Some(ToolOutputDisplayRecord::InspectRead(vec![
                        InspectReadDisplayRecord {
                            path: "src/\u{202e}lib.rs".to_string(),
                            start_line: 7,
                            lines: vec!["fn main() {}".to_string()],
                            next: Some(InspectReadNextRecord {
                                start_line: 8,
                                line_count: 12,
                            }),
                        },
                    ])),
                }),
            },
            UiTranscriptEntry::SessionRecord {
                source_sequence: Some(7),
                record: SessionRecordKind::FunctionToolCall(FunctionToolCallRecord {
                    call_id: "function-call".to_string(),
                    name: "function".to_string(),
                    arguments: "{\"value\":1}".to_string(),
                }),
            },
            UiTranscriptEntry::SessionRecord {
                source_sequence: Some(8),
                record: SessionRecordKind::FunctionToolOutput(FunctionToolOutputRecord {
                    call_id: "function-call".to_string(),
                    output: "function model output".to_string(),
                    display_output: Some("function display output".to_string()),
                }),
            },
            UiTranscriptEntry::SessionRecord {
                source_sequence: Some(9),
                record: SessionRecordKind::SessionClosed(SessionClosedRecord {
                    closed_at_ms: 123_456,
                }),
            },
        ];
        let prompt = "\u{1b}[31m\r\nexact".to_string();
        let snapshot = UiSnapshot {
            session_id: "session-\u{202e}".to_string(),
            thread_title: "thread \u{1b}]0;title\u{7}".to_string(),
            provider: Some(ProviderUiInfo {
                display_name: "custom \u{1b}[34mprovider".to_string(),
                kind: ProviderKind::HttpsApi,
                transport: ProviderTransportKind::Https,
            }),
            model_settings: ModelSettings {
                model: "model-\u{202e}".to_string(),
                reasoning_effort: Some("default".to_string()),
                service_tier: Some("default".to_string()),
            },
            developer_mode: true,
            response_streaming: true,
            last_ttft_ms: Some(42),
            transcript_entries: transcript_entries.clone(),
            input_cursor: prompt.len(),
            input: prompt.clone(),
            queued_steering_prompt: Some("queued\r\n\u{1b}".to_string()),
            agents: vec![AgentSummary {
                id: AgentId(17),
                path: "agent/\u{202e}".to_string(),
                status: AgentStatus::Completed("done\u{1b}".to_string()),
                last_task_message: Some("task\r\n".to_string()),
                last_activity_message: Some("activity\u{7}".to_string()),
            }],
            active_activities: vec!["activity-\u{202e}".to_string()],
        };

        let application = Application::import(import_snapshot(snapshot)).unwrap();
        let exported = export_snapshot(application.into_final_state());

        assert_eq!(exported.session_id, "session-\u{202e}");
        assert_eq!(exported.thread_title, "thread \u{1b}]0;title\u{7}");
        assert_eq!(
            exported.provider,
            Some(ProviderUiInfo {
                display_name: "custom \u{1b}[34mprovider".to_string(),
                kind: ProviderKind::HttpsApi,
                transport: ProviderTransportKind::Https,
            })
        );
        assert_eq!(
            exported.model_settings,
            ModelSettings {
                model: "model-\u{202e}".to_string(),
                reasoning_effort: Some("default".to_string()),
                service_tier: Some("default".to_string()),
            }
        );
        assert!(exported.developer_mode);
        assert!(exported.response_streaming);
        assert_eq!(exported.last_ttft_ms, Some(42));
        assert_eq!(exported.transcript_entries, transcript_entries);
        assert_eq!(exported.input, prompt);
        assert_eq!(exported.input_cursor, exported.input.len());
        assert_eq!(
            exported.queued_steering_prompt.as_deref(),
            Some("queued\r\n\u{1b}")
        );
        assert_eq!(exported.active_activities, vec!["activity-\u{202e}"]);
        assert_eq!(exported.agents.len(), 1);
        let agent = &exported.agents[0];
        assert_eq!(agent.id, AgentId(17));
        assert_eq!(agent.path, "agent/\u{202e}");
        assert_eq!(
            agent.status,
            AgentStatus::Completed("done\u{1b}".to_string())
        );
        assert_eq!(agent.last_task_message.as_deref(), Some("task\r\n"));
        assert_eq!(
            agent.last_activity_message.as_deref(),
            Some("activity\u{7}")
        );
    }

    #[test]
    fn model_option_round_trip_distinguishes_absence_from_default_text() {
        for (reasoning_effort, service_tier) in [
            (None, None),
            (Some("default".to_string()), Some("default".to_string())),
        ] {
            let expected = ModelSettings {
                model: "model".to_string(),
                reasoning_effort,
                service_tier,
            };
            let application = Application::import(import_snapshot(snapshot_with_model_settings(
                expected.clone(),
            )))
            .unwrap();

            let exported = export_snapshot(application.into_final_state());
            assert_eq!(exported.model_settings, expected);
        }
    }

    #[test]
    fn transcript_page_filters_hidden_records_without_losing_sequence_identity() {
        let page = TranscriptPage {
            lines: vec![
                TranscriptPageLine {
                    seq: 10,
                    kind: SessionRecordKind::TurnContext(TurnContextRecord {
                        cwd: "/workspace".to_string(),
                        date: "2026-01-01".to_string(),
                        timezone: "UTC".to_string(),
                        model: "model".to_string(),
                        sandbox_policy: "sandbox".to_string(),
                        approval_policy: "approval".to_string(),
                    }),
                },
                TranscriptPageLine {
                    seq: 11,
                    kind: SessionRecordKind::FreeformToolInputDelta(FreeformToolInputDeltaRecord {
                        item_id: "item".to_string(),
                        delta: "partial".to_string(),
                    }),
                },
                TranscriptPageLine {
                    seq: 12,
                    kind: SessionRecordKind::AssistantMessage(MessageRecord {
                        text: "visible".to_string(),
                    }),
                },
            ],
            next_before_seq: Some(10),
            reached_start: false,
        };

        let DomainEvent::TranscriptPage {
            entries,
            next_before_sequence,
            reached_start,
        } = adapt_runtime_event(RuntimeEvent::TranscriptPage(page))
        else {
            panic!("transcript page adapts to a domain page");
        };

        assert_eq!(next_before_sequence, Some(10));
        assert!(!reached_start);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sequence, 12);
        assert!(matches!(
            &entries[0].payload,
            TranscriptPayload::Message { text, .. } if text.as_str() == "visible"
        ));
    }

    #[test]
    fn unknown_activity_status_is_rejected_at_the_runtime_boundary() {
        let event = adapt_runtime_event(RuntimeEvent::SubagentActivity {
            activity_id: "worker-1".to_string(),
            description: "worker".to_string(),
            status: "paused".to_string(),
            detail: None,
        });
        let DomainEvent::ProtocolViolation(message) = event else {
            panic!("unknown activity status must be rejected");
        };

        assert!(message.as_str().contains("worker-1"));
        assert!(message.as_str().contains("\"paused\""));

        let DomainEvent::ActivityChanged(activity) =
            adapt_runtime_event(RuntimeEvent::SubagentActivity {
                activity_id: "worker-1".to_string(),
                description: "worker".to_string(),
                status: "running".to_string(),
                detail: Some("detail".to_string()),
            })
        else {
            panic!("known activity status must be adapted");
        };
        assert_eq!(activity.status, ActivityStatus::Running);
    }

    #[test]
    fn runtime_requests_preserve_exact_payloads_and_adapter_owned_page_policy() {
        let RuntimeCommand::ApplySteering { text, mode } =
            export_runtime_request(RuntimeRequest::ApplySteering {
                text: "\r\n\u{1b}exact".to_string(),
            })
        else {
            panic!("immediate steering exports to ApplySteering");
        };
        assert_eq!(text, "\r\n\u{1b}exact");
        assert_eq!(mode, SteeringMode::InterruptNow);

        let RuntimeCommand::LoadTranscriptPage {
            before_seq,
            max_lines,
        } = export_runtime_request(RuntimeRequest::LoadTranscriptPage {
            before_sequence: Some(73),
        })
        else {
            panic!("page request exports to LoadTranscriptPage");
        };
        assert_eq!(before_seq, Some(73));
        assert_eq!(max_lines, CORE_TRANSCRIPT_PAGE_ENTRY_LIMIT);
    }
}
