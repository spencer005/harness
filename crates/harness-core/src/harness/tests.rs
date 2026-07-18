use std::fs;

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value, json};

use super::*;
use crate::{
    responses::lean_codex_default_headers,
    sessions::{FreeformToolOutputRecord, SessionRecord, SessionStore},
    tools::{
        APPLY_PATCH_TOOL_NAME, EDIT_FILE_TOOL_NAME, INSPECT_TOOL_NAME, LOCATE_TOOL_NAME,
        MARK_TASK_COMPLETE_TOOL_NAME, TERMINAL_OPEN_TOOL_NAME, TERMINAL_WRITE_TOOL_NAME,
    },
};

fn temp_root(name: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "new-harness-runtime-{name}-{}-{now}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create temp root");
    root
}

fn large_terminal_text() -> String {
    let mut text = String::new();
    for index in 0..5_000 {
        text.push_str(&format!("line-{index:04} summary payload\n"));
    }
    text
}

fn test_config(root: PathBuf) -> HarnessConfig {
    let session_id = SessionId::new("harness-session");
    let profile = crate::providers::ProviderProfile {
        id: crate::providers::ProviderProfileId::new("test"),
        display_name: "Test".to_string(),
        kind: crate::providers::ProviderKind::Codex,
        auth: crate::providers::ProviderAuthConfig::ChatGptHarness,
        driver: crate::providers::ProviderDriverConfig::CodexWsResponses {
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            stream_idle_timeout_ms: 300_000,
        },
        default_model: "gpt-test".to_string(),
        default_reasoning_effort: Some("xhigh".to_string()),
        default_service_tier: Some("priority".to_string()),
        model_configs: Vec::new(),
        tool_output_summary_model: "gpt-5.4".to_string(),
    };
    let api = crate::responses::ApiProvider::new(profile.driver.base_url())
        .expect("test base URL is valid")
        .with_stream_idle_timeout(profile.driver.stream_idle_timeout());
    let auth = crate::provider_runtime::ProviderAuthRuntime::ApiKey("test".to_string());
    let model_catalog = crate::provider_runtime::ProviderModelCatalog {
        profile_id: profile.id.clone(),
        models: vec![
            crate::provider_runtime::ProviderModelEntry {
                info: ResponsesModelInfo {
                    slug: "gpt-test".to_string(),
                    supports_parallel_tool_calls: true,
                    supports_reasoning: true,
                    context_window: Some(272_000),
                    max_context_window: Some(1_000_000),
                    auto_compact_token_limit: None,
                    effective_context_window_percent: 95,
                },
                config: None,
                capabilities: ResponsesModelCapabilities::new(true, true),
            },
            crate::provider_runtime::ProviderModelEntry {
                info: ResponsesModelInfo {
                    slug: "gpt-5.5".to_string(),
                    supports_parallel_tool_calls: true,
                    supports_reasoning: true,
                    context_window: Some(272_000),
                    max_context_window: Some(1_000_000),
                    auto_compact_token_limit: None,
                    effective_context_window_percent: 95,
                },
                config: None,
                capabilities: ResponsesModelCapabilities::new(true, true),
            },
        ],
        etag: None,
        fetched_at: std::time::SystemTime::now(),
    };
    let selected_model = crate::responses::ModelSettings::new(
        "gpt-test",
        Some("xhigh".to_string()),
        Some("priority".to_string()),
    );
    let context_window_policy =
        crate::responses::context_window_policy_from_model_info(&model_catalog.models[0].info)
            .expect("test model has a valid context window policy");
    let provider_runtime = crate::provider_runtime::ProviderRuntime {
        profile: profile.clone(),
        api: api.clone(),
        default_headers: lean_codex_default_headers(),
        auth,
        model_catalog: model_catalog.clone(),
        selected_model: selected_model.clone(),
        selected_capabilities: ResponsesModelCapabilities::new(true, true),
        context_window_policy,
    };
    HarnessConfig {
        session_root: root,
        session_id: session_id.clone(),
        resume_session_path: None,
        initial_transcript_before_seq: None,
        cwd: "/tmp/project".to_string(),
        provider_runtime,
        provider: crate::providers::ProviderUiInfo::from_profile(&profile),
        provider_store: None,
        credential_store: None,
        codex_home: None,
        model: "gpt-test".to_string(),
        reasoning_effort: Some("xhigh".to_string()),
        service_tier: Some("priority".to_string()),
        developer_mode: DEFAULT_DEVELOPER_MODE,
        instructions: "Be concise.".to_string(),
        source: "test".to_string(),
        originator: "new_harness".to_string(),
        client_version: "0.1.0".to_string(),
        sandbox_policy: "workspace-write".to_string(),
        approval_policy: "never".to_string(),
        responses_headers: ResponsesHeaders::for_thread(
            session_id.as_str(),
            session_id.as_str(),
            "harness-session:0",
        ),
        model_capabilities: ResponsesModelCapabilities::new(true, true),
        model_catalog: model_catalog.raw_models(),
        tool_registry: NativeToolRegistry::apply_patch_only(),
        terminal_tools_enabled: true,
        context_window_policy: Some(context_window_policy),
        analytics: None,
    }
}

fn terminal_tool_registry() -> NativeToolRegistry {
    NativeToolRegistry::codex()
}

fn add_small_context_model(config: &mut HarnessConfig) {
    let info = ResponsesModelInfo {
        slug: "gpt-small".to_string(),
        supports_parallel_tool_calls: true,
        supports_reasoning: true,
        context_window: Some(20),
        max_context_window: Some(20),
        auto_compact_token_limit: None,
        effective_context_window_percent: 100,
    };
    config.model_catalog.push(info.clone());
    config.provider_runtime.model_catalog.models.push(
        crate::provider_runtime::ProviderModelEntry {
            info,
            config: None,
            capabilities: ResponsesModelCapabilities::new(true, true),
        },
    );
}

fn message_input_text(item: &Value) -> &str {
    item.as_object()
        .unwrap()
        .get(&"content")
        .and_then(|value| value.as_array())
        .unwrap()[0]
        .as_object()
        .unwrap()
        .get(&"text")
        .and_then(JsonValueTrait::as_str)
        .unwrap()
}

fn response_tool_names(request: &ResponsesRequest) -> Vec<String> {
    request
        .body
        .as_object()
        .unwrap()
        .get(&"tools")
        .and_then(|value| value.as_array())
        .unwrap()
        .iter()
        .filter_map(|tool| {
            tool.as_object()
                .unwrap()
                .get(&"name")
                .and_then(JsonValueTrait::as_str)
                .map(str::to_string)
        })
        .collect()
}

async fn next_responses_submit_matching(
    responses_rx: &ActorReceiver<ResponsesCommand>,
    mut matches: impl FnMut(&ResponsesRequest) -> bool,
    label: &str,
) -> (ResponsesRequest, ActorSender<ResponsesEvent>) {
    loop {
        let command = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
            .unwrap();
        let ResponsesCommand::Submit { request, events } = command else {
            panic!("expected submit command for {label}");
        };
        if matches(&request) {
            return (request, events);
        }
    }
}

async fn next_responses_submit(
    responses_rx: &ActorReceiver<ResponsesCommand>,
    label: &str,
) -> (ResponsesRequest, ActorSender<ResponsesEvent>) {
    next_responses_submit_matching(responses_rx, |_| true, label).await
}

async fn next_responses_close_idle(responses_rx: &ActorReceiver<ResponsesCommand>, label: &str) {
    let command = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .unwrap();
    let ResponsesCommand::CloseIdle = command else {
        panic!("expected close idle command for {label}");
    };
}

async fn skip_provider_changed_event(runtime: &HarnessRuntime) {
    let event = runtime.events.recv().await.unwrap();
    assert!(
        matches!(event, RuntimeEvent::ProviderChanged(_)),
        "expected initial ProviderChanged event, got {event:?}"
    );
}

async fn next_runtime_event_matching(
    runtime: &HarnessRuntime,
    mut matches: impl FnMut(&RuntimeEvent) -> bool,
) -> RuntimeEvent {
    let mut ignored_events = Vec::new();
    loop {
        let event = match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            runtime.events.recv(),
        )
        .await
        {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => panic!("runtime event stream closed: {error}"),
            Err(error) => panic!(
                "timed out waiting for matching runtime event after {error}; ignored events: {ignored_events:?}"
            ),
        };
        if matches(&event) {
            return event;
        }
        ignored_events.push(format!("{event:?}"));
    }
}

async fn assert_runtime_error_containing(runtime: &HarnessRuntime, needle: &str) {
    let mut found = false;
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(50), runtime.events.recv())
            .await
        {
            Ok(Ok(RuntimeEvent::TranscriptLine(line))) => {
                if line.starts_with("error:") && line.contains(needle) {
                    found = true;
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => panic!("runtime event stream closed: {error}"),
            Err(_) => break,
        }
    }
    assert!(
        found,
        "expected runtime error containing '{}' but none was found",
        needle
    );
}

#[tokio::test]
async fn harness_resume_uses_persisted_history_for_first_request() {
    let root = temp_root("resume-history-first-request");
    let store = SessionStore::new(&root);
    let mut writer = store
        .create_session(SessionMeta {
            id: SessionId::new("harness-session"),
            parent_session_id: None,
            forked_from_session_id: None,
            created_at_ms: 1_717_171_717_000,
            updated_at_ms: 1_717_171_717_000,
            cwd: "/tmp/project".to_string(),
            model: "gpt-test".to_string(),
            originator: "new_harness".to_string(),
            client_version: "0.1.0".to_string(),
            source: "test".to_string(),
            title: Some("resume".to_string()),
            preview: Some("old user".to_string()),
        })
        .unwrap();
    writer.append_many([
        SessionRecordKind::UserMessage(MessageRecord {
            text: "old user".to_string(),
        }),
        SessionRecordKind::AssistantMessage(MessageRecord {
            text: "old assistant".to_string(),
        }),
    ]);
    writer.flush().unwrap();
    let resume_path = writer.path().to_path_buf();
    drop(writer);

    let mut config = test_config(root);
    config.resume_session_path = Some(resume_path);
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "new input".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, .. } = responses_rx.recv().await.unwrap() else {
        panic!("expected submit command");
    };
    let input = request
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(input.len(), 3);
}

#[tokio::test]
async fn transcript_page_request_starts_before_live_transcript() {
    let root = temp_root("transcript-page-live-cursor");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "visible live line".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit { .. } = responses_rx.recv().await.unwrap() else {
        panic!("expected submit command");
    };

    runtime
        .commands
        .send(RuntimeCommand::LoadTranscriptPage {
            before_seq: None,
            max_lines: 96,
        })
        .await
        .unwrap();

    let RuntimeEvent::TranscriptPage(page) = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::TranscriptPage(_))
    })
    .await
    else {
        panic!("expected transcript page");
    };

    assert_eq!(page.lines, Vec::new());
    assert_eq!(page.next_before_seq, None);
    assert_eq!(page.reached_start, true);
}

#[tokio::test]
async fn toolsoverride_compat_advertises_function_wrapped_tools() {
    let root = temp_root("toolsoverride-compat");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    skip_provider_changed_event(&runtime).await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/toolsoverride compat".to_string(),
        })
        .await
        .unwrap();

    let event = runtime.events.recv().await.unwrap();
    let RuntimeEvent::TranscriptLine(line) = event else {
        panic!("expected transcript line");
    };
    assert_eq!(line, "tools override: compat");
    assert!(responses_rx.try_recv().is_err());

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "hello".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, .. } = responses_rx.recv().await.unwrap() else {
        panic!("expected submit command");
    };
    let tools = request
        .body
        .as_object()
        .unwrap()
        .get(&"tools")
        .and_then(|value| value.as_array())
        .unwrap();
    assert!(tools.iter().all(|tool| {
        tool.as_object()
            .unwrap()
            .get(&"type")
            .and_then(JsonValueTrait::as_str)
            == Some("function")
    }));
}

#[tokio::test]
async fn toolsoverride_custom_advertises_native_custom_tools() {
    let root = temp_root("toolsoverride-custom");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let mut config = test_config(root);
    config.tool_registry = NativeToolRegistry::ollama_cloud();
    let runtime = HarnessActor::spawn(config, responses);

    skip_provider_changed_event(&runtime).await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/toolsoverride custom".to_string(),
        })
        .await
        .unwrap();

    let event = runtime.events.recv().await.unwrap();
    let RuntimeEvent::TranscriptLine(line) = event else {
        panic!("expected transcript line");
    };
    assert_eq!(line, "tools override: custom");
    assert!(responses_rx.try_recv().is_err());

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "hello".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, .. } = responses_rx.recv().await.unwrap() else {
        panic!("expected submit command");
    };
    let tools = request
        .body
        .as_object()
        .unwrap()
        .get(&"tools")
        .and_then(|value| value.as_array())
        .unwrap();
    assert!(tools.iter().any(|tool| {
        tool.as_object()
            .unwrap()
            .get(&"type")
            .and_then(JsonValueTrait::as_str)
            == Some("custom")
    }));
}

#[tokio::test]
async fn harness_persists_assistant_text_when_response_completes() {
    let root = temp_root("assistant-complete");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root.clone()), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "hello".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::Frame {
            request_id: request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "hi back"
            }),
        })
        .await
        .unwrap();
    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();

    let mut saw_ttft = false;
    let mut saw_delta = false;
    for _ in 0..12 {
        if saw_ttft && saw_delta {
            break;
        }
        let Ok(Ok(event)) =
            tokio::time::timeout(std::time::Duration::from_secs(1), runtime.events.recv()).await
        else {
            break;
        };
        match event {
            RuntimeEvent::AssistantFirstToken { .. } => saw_ttft = true,
            RuntimeEvent::AssistantTextDelta(delta) if delta == "hi back" => saw_delta = true,
            _ => {}
        }
    }
    assert!(saw_ttft);
    assert!(saw_delta);

    let summary = {
        let mut index = crate::sessions::SessionIndex::load(&root).unwrap();
        index
            .summary_by_id(&SessionId::new("harness-session"))
            .unwrap()
            .unwrap()
    };
    let records = SessionStore::new(&root).read_records(summary.path).unwrap();
    assert_eq!(
        records.last().unwrap().kind,
        SessionRecordKind::AssistantMessage(MessageRecord {
            text: "hi back".to_string()
        })
    );
}
#[tokio::test]
async fn root_agentic_loop_events_wrap_simple_response() {
    let root = temp_root("root-agentic-loop-simple");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "start".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial submit").await;
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::AgenticLoopStarted)
    })
    .await;

    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();

    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::AgenticLoopCompleted)
    })
    .await;
}

#[tokio::test]
async fn root_agentic_loop_stays_active_between_tool_call_and_continuation() {
    let root = temp_root("root-agentic-loop-tool-gap");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "use a tool".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial submit").await;
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::AgenticLoopStarted)
    })
    .await;

    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: crate::tools::FreeformToolCall {
                call_id: "call-gap".to_string(),
                name: APPLY_PATCH_TOOL_NAME.to_string(),
                input: "not a patch".to_string(),
            },
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamCompleted)
    })
    .await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            next_runtime_event_matching(&runtime, |event| {
                matches!(event, RuntimeEvent::AgenticLoopCompleted)
            })
        )
        .await
        .is_err()
    );

    let (continuation, continuation_events) =
        next_responses_submit(&responses_rx, "tool continuation").await;
    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();
    continuation_events
        .send(ResponsesEvent::RequestCompleted(continuation.request_id))
        .await
        .unwrap();

    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::AgenticLoopCompleted)
    })
    .await;
}

#[tokio::test]
async fn root_empty_interrupt_abandons_active_response_without_continuation() {
    let root = temp_root("root-empty-interrupt");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "start".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamStarted)
    })
    .await;

    runtime
        .commands
        .send(RuntimeCommand::ApplySteering {
            text: String::new(),
            mode: SteeringMode::InterruptNow,
        })
        .await
        .unwrap();

    let RuntimeEvent::TranscriptLine(line) = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::TranscriptLine(line) if line == "interrupt requested")
        })
        .await
        else {
            panic!("expected interrupt transcript");
        };
    assert_eq!(line, "interrupt requested");
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamCompleted)
    })
    .await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), responses_rx.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn root_interrupt_steering_abandons_active_response_and_continues() {
    let root = temp_root("root-live-steering");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "start answer".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::Frame {
            request_id: request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "partial answer"
            }),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::AssistantTextDelta(delta) if delta == "partial answer")
        })
        .await;

    runtime
        .commands
        .send(RuntimeCommand::ApplySteering {
            text: "focus on the bug".to_string(),
            mode: SteeringMode::InterruptNow,
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::TranscriptLine(line) if line == "steering: focus on the bug")
        })
        .await;
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamCompleted)
    })
    .await;

    let ResponsesCommand::Submit {
        request: continuation,
        ..
    } = responses_rx.recv().await.unwrap()
    else {
        panic!("expected steering continuation submit");
    };
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert!(
        input
            .iter()
            .any(|item| message_input_text(item) == "partial answer")
    );

    events
        .send(ResponsesEvent::Frame {
            request_id: request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "late output must be ignored"
            }),
        })
        .await
        .unwrap();
    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();
    assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                next_runtime_event_matching(&runtime, |event| {
                    matches!(event, RuntimeEvent::AssistantTextDelta(delta) if delta == "late output must be ignored")
                })
            )
            .await
                .is_err()
        );
}

#[tokio::test]
async fn root_queued_steering_appends_without_submitting_or_transcript() {
    let root = temp_root("root-queued-steering-appends");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "start answer".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamStarted)
    })
    .await;

    runtime
        .commands
        .send(RuntimeCommand::QueueSteering {
            text: "first".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::SteeringQueued(Some(text)) if text == "first"),
    )
    .await;
    runtime
        .commands
        .send(RuntimeCommand::QueueSteering {
            text: " second ".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::SteeringQueued(Some(text)) if text == "first\nsecond")
        })
        .await;

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), responses_rx.recv())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), runtime.events.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn root_submit_input_during_active_response_queues_steering() {
    let root = temp_root("root-submit-input-during-active-response");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "start answer".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamStarted)
    })
    .await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "steer from submit".to_string(),
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::SteeringQueued(Some(text)) if text == "steer from submit")
        })
        .await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), responses_rx.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn root_queued_steering_interrupt_sends_queued_text_once() {
    let root = temp_root("root-queued-steering-interrupt");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "start answer".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamStarted)
    })
    .await;

    runtime
        .commands
        .send(RuntimeCommand::QueueSteering {
            text: "first".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::SteeringQueued(Some(text)) if text == "first"),
    )
    .await;
    runtime
        .commands
        .send(RuntimeCommand::ApplySteering {
            text: "first".to_string(),
            mode: SteeringMode::InterruptNow,
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "steering: first"),
    )
    .await;
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::SteeringQueued(None))
    })
    .await;

    let ResponsesCommand::Submit { .. } = responses_rx.recv().await.unwrap() else {
        panic!("expected steering continuation submit");
    };
}

#[tokio::test]
async fn provider_switch_during_active_response_applies_after_turn_completion() {
    let root = temp_root("provider-switch-during-active-response");
    let credential_store = Arc::new(crate::provider_runtime::FileProviderCredentialStore::new(
        root.join("credentials"),
    ));
    let credential_id = crate::provider_runtime::ProviderCredentialStore::save_api_key(
        credential_store.as_ref(),
        "alternate".to_string(),
        "test-key".to_string(),
    )
    .await
    .unwrap();

    let mut config = test_config(root.clone());
    let alternate_profile = crate::providers::ProviderProfile {
        id: crate::providers::ProviderProfileId::new("alternate"),
        display_name: "Alternate".to_string(),
        kind: crate::providers::ProviderKind::HttpsApi,
        auth: crate::providers::ProviderAuthConfig::ApiKey { credential_id },
        driver: crate::providers::ProviderDriverConfig::HttpsResponses {
            base_url: "https://provider.example/v1".to_string(),
            request_timeout_ms: 30_000,
            stream_idle_timeout_ms: 300_000,
        },
        default_model: "alternate-model".to_string(),
        default_reasoning_effort: None,
        default_service_tier: None,
        model_configs: vec![crate::providers::ProviderModelConfig {
            slug: "alternate-model".to_string(),
            context_window: Some(128_000),
            effective_context_window_percent: 95,
            supports_tools: true,
            supports_parallel_tool_calls: true,
            service_tiers: Vec::new(),
        }],
        tool_output_summary_model: "alternate-model".to_string(),
    };
    let provider_store = Arc::new(crate::providers::FileProviderConfigStore::new(
        root.join("providers.json"),
    ));
    crate::providers::ProviderConfigStore::save(
        provider_store.as_ref(),
        &crate::providers::ProviderConfig {
            default_profile_id: config.provider_runtime.profile.id.clone(),
            profiles: vec![
                config.provider_runtime.profile.clone(),
                alternate_profile.clone(),
            ],
        },
    )
    .unwrap();
    config.provider_store = Some(provider_store);
    config.credential_store = Some(credential_store);

    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);
    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "start answer".to_string(),
        })
        .await
        .unwrap();
    let (request, events) = next_responses_submit(&responses_rx, "initial submit").await;
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamStarted)
    })
    .await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/provider use alternate".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(
            event,
            RuntimeEvent::TranscriptLine(line)
                if line == "provider switch queued: alternate"
        )
    })
    .await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), responses_rx.recv())
            .await
            .is_err(),
        "active provider transport shut down before the response completed"
    );

    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();
    let RuntimeEvent::ProviderChanged(provider) = next_runtime_event_matching(&runtime, |event| {
        matches!(
            event,
            RuntimeEvent::ProviderChanged(provider)
                if provider.display_name == alternate_profile.display_name
        )
    })
    .await
    else {
        panic!("expected alternate provider change");
    };
    assert_eq!(provider.display_name, "Alternate");

    let command = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .expect("timed out waiting for old provider shutdown")
        .unwrap();
    assert!(matches!(command, ResponsesCommand::Shutdown));
}

#[tokio::test]
async fn root_websocket_connection_limit_error_closes_idle_and_retries() {
    let root = temp_root("root-websocket-limit-retry");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "continue after websocket reconnect".to_string(),
        })
        .await
        .unwrap();

    let (failed_request, failed_events) =
        next_responses_submit(&responses_rx, "failed root submit").await;
    failed_events
        .send(ResponsesEvent::RequestStarted(failed_request.request_id))
        .await
        .unwrap();
    failed_events
        .send(ResponsesEvent::Error {
            request_id: Some(failed_request.request_id),
            message: "retryable websocket error: Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue.".to_string(),
        })
        .await
        .unwrap();

    assert_runtime_error_containing(&runtime, "Responses websocket connection limit reached").await;

    next_responses_close_idle(&responses_rx, "root reconnect close idle").await;
    let (retry_request, retry_events) =
        next_responses_submit(&responses_rx, "retry root submit").await;
    assert_ne!(retry_request.request_id, failed_request.request_id);

    retry_events
        .send(ResponsesEvent::RequestStarted(retry_request.request_id))
        .await
        .unwrap();
    retry_events
        .send(ResponsesEvent::Frame {
            request_id: retry_request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "reconnected"
            }),
        })
        .await
        .unwrap();
    retry_events
        .send(ResponsesEvent::RequestCompleted(retry_request.request_id))
        .await
        .unwrap();

    let completed = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamCompleted)
    })
    .await;
    assert!(matches!(completed, RuntimeEvent::ResponseStreamCompleted));
}
#[tokio::test]
async fn root_connection_body_read_error_closes_idle_and_retries() {
    let root = temp_root("root-body-read-retry");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);
    let failure_message = "error reading a body from connection";

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "continue after connection body read failure".to_string(),
        })
        .await
        .unwrap();

    let (failed_request, failed_events) =
        next_responses_submit(&responses_rx, "failed root submit").await;
    failed_events
        .send(ResponsesEvent::RequestStarted(failed_request.request_id))
        .await
        .unwrap();
    failed_events
        .send(ResponsesEvent::Error {
            request_id: Some(failed_request.request_id),
            message: failure_message.to_string(),
        })
        .await
        .unwrap();

    assert_runtime_error_containing(&runtime, failure_message).await;
    next_responses_close_idle(&responses_rx, "body read error close idle").await;
    let (retry_request, retry_events) =
        next_responses_submit(&responses_rx, "body read error retry submit").await;
    assert_ne!(retry_request.request_id, failed_request.request_id);

    retry_events
        .send(ResponsesEvent::RequestStarted(retry_request.request_id))
        .await
        .unwrap();
    retry_events
        .send(ResponsesEvent::RequestCompleted(retry_request.request_id))
        .await
        .unwrap();

    let completed = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamCompleted)
    })
    .await;
    assert!(matches!(completed, RuntimeEvent::ResponseStreamCompleted));
}

#[tokio::test]
async fn compaction_websocket_connection_limit_error_closes_idle_and_retries() {
    let root = temp_root("compact-websocket-limit-retry");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "remember this before compacting".to_string(),
        })
        .await
        .unwrap();

    let (initial_request, initial_events) =
        next_responses_submit(&responses_rx, "initial submit before compact").await;
    initial_events
        .send(ResponsesEvent::RequestStarted(initial_request.request_id))
        .await
        .unwrap();
    initial_events
        .send(ResponsesEvent::Frame {
            request_id: initial_request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "assistant memory before compact"
            }),
        })
        .await
        .unwrap();
    initial_events
        .send(ResponsesEvent::RequestCompleted(initial_request.request_id))
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamCompleted)
    })
    .await;

    runtime
        .commands
        .send(RuntimeCommand::Compact {
            request: CompactRequest::new(
                "compact reconnect test",
                vec![json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{
                        "type": "input_text",
                        "text": "remember this before compacting"
                    }]
                })],
            ),
        })
        .await
        .unwrap();

    let (failed_compact, failed_events) =
        next_responses_submit(&responses_rx, "failed compact submit").await;
    failed_events
        .send(ResponsesEvent::RequestStarted(failed_compact.request_id))
        .await
        .unwrap();
    failed_events
        .send(ResponsesEvent::Error {
            request_id: Some(failed_compact.request_id),
            message: "retryable websocket error: Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue.".to_string(),
        })
        .await
        .unwrap();

    assert_runtime_error_containing(&runtime, "Responses websocket connection limit reached").await;

    next_responses_close_idle(&responses_rx, "compact reconnect close idle").await;
    let (retry_compact, retry_events) =
        next_responses_submit(&responses_rx, "retry compact submit").await;
    assert_ne!(retry_compact.request_id, failed_compact.request_id);

    retry_events
        .send(ResponsesEvent::RequestStarted(retry_compact.request_id))
        .await
        .unwrap();
    retry_events
        .send(ResponsesEvent::Frame {
            request_id: retry_compact.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "summary after reconnect"
            }),
        })
        .await
        .unwrap();
    retry_events
        .send(ResponsesEvent::RequestCompleted(retry_compact.request_id))
        .await
        .unwrap();

    let compact_event = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::CompactCompleted(_))
    })
    .await;
    let RuntimeEvent::CompactCompleted(result) = compact_event else {
        panic!("expected compact completed event");
    };
    assert_eq!(result.summary, "summary after reconnect");

    let (continuation, _) = next_responses_submit(&responses_rx, "post-compact continuation").await;
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(input.len(), 1);
    assert!(message_input_text(&input[0]).contains("summary after reconnect"));
}

#[tokio::test]
async fn compaction_websocket_connection_limit_keeps_retrying_without_error() {
    let root = temp_root("compact-websocket-limit-repeat-retry");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "prepare repeated compaction retry".to_string(),
        })
        .await
        .unwrap();

    let (initial_request, initial_events) =
        next_responses_submit(&responses_rx, "initial submit before repeated compact").await;
    initial_events
        .send(ResponsesEvent::RequestStarted(initial_request.request_id))
        .await
        .unwrap();
    initial_events
        .send(ResponsesEvent::RequestCompleted(initial_request.request_id))
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamCompleted)
    })
    .await;

    runtime
        .commands
        .send(RuntimeCommand::Compact {
            request: CompactRequest::new(
                "compact repeated reconnect test",
                vec![json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{
                        "type": "input_text",
                        "text": "durable repeated compact input"
                    }]
                })],
            ),
        })
        .await
        .unwrap();

    let connection_limit_message = "retryable websocket error: Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue.";

    let (first_failed, first_events) =
        next_responses_submit(&responses_rx, "first failed compact submit").await;
    first_events
        .send(ResponsesEvent::RequestStarted(first_failed.request_id))
        .await
        .unwrap();
    first_events
        .send(ResponsesEvent::Error {
            request_id: Some(first_failed.request_id),
            message: connection_limit_message.to_string(),
        })
        .await
        .unwrap();

    assert_runtime_error_containing(&runtime, "Responses websocket connection limit reached").await;

    next_responses_close_idle(&responses_rx, "first compact reconnect close idle").await;
    let (second_failed, second_events) =
        next_responses_submit(&responses_rx, "second failed compact submit").await;
    assert_ne!(second_failed.request_id, first_failed.request_id);
    second_events
        .send(ResponsesEvent::RequestStarted(second_failed.request_id))
        .await
        .unwrap();
    second_events
        .send(ResponsesEvent::Error {
            request_id: Some(second_failed.request_id),
            message: connection_limit_message.to_string(),
        })
        .await
        .unwrap();

    assert_runtime_error_containing(&runtime, "Responses websocket connection limit reached").await;

    next_responses_close_idle(&responses_rx, "second compact reconnect close idle").await;
    let (retry_compact, retry_events) =
        next_responses_submit(&responses_rx, "third compact submit").await;
    assert_ne!(retry_compact.request_id, second_failed.request_id);

    retry_events
        .send(ResponsesEvent::RequestStarted(retry_compact.request_id))
        .await
        .unwrap();
    retry_events
        .send(ResponsesEvent::Frame {
            request_id: retry_compact.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "summary after repeated reconnect"
            }),
        })
        .await
        .unwrap();
    retry_events
        .send(ResponsesEvent::RequestCompleted(retry_compact.request_id))
        .await
        .unwrap();

    let compact_event = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::CompactCompleted(_))
    })
    .await;
    let RuntimeEvent::CompactCompleted(result) = compact_event else {
        panic!("expected compact completed event");
    };
    assert_eq!(result.summary, "summary after repeated reconnect");
}

#[tokio::test]
async fn manual_compact_waits_for_active_root_response_terminal_event() {
    let root = temp_root("manual-compact-active-root-response");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "remember active alpha".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::Frame {
            request_id: request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "active beta"
            }),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::AssistantTextDelta(delta) if delta == "active beta"),
    )
    .await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/compact".to_string(),
        })
        .await
        .unwrap();
    runtime
        .commands
        .send(RuntimeCommand::QueueSteering {
            text: "drain after queued manual compact".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::SteeringQueued(Some(text)) if text == "drain after queued manual compact")
        })
        .await;

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), responses_rx.recv())
            .await
            .is_err()
    );

    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();

    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamCompleted)
    })
    .await;
    let ResponsesCommand::Submit {
        request: compact_request,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected compact submit command");
    };
    let compact_input = compact_request
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert!(
        compact_input
            .iter()
            .any(|item| message_input_text(item) == "active beta")
    );
}

#[test]
fn instructions_append_escaped_cwd_environment_context() {
    assert_eq!(
        instructions_with_cwd("Base.", "/tmp/a&b/<repo>"),
        "Base.\n\n<environment_context>\n  <cwd>/tmp/a&amp;b/&lt;repo&gt;</cwd>\n</environment_context>"
    );
}

#[tokio::test]
async fn context_window_error_compacts_in_two_blocks_and_retries() {
    let root = temp_root("context-error-compact");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "first durable fact".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected first submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::Frame {
            request_id: request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "first assistant fact"
            }),
        })
        .await
        .unwrap();
    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ResponseStreamCompleted)
    })
    .await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "second durable fact".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit {
        request: failed_request,
        events: failed_events,
    } = responses_rx.recv().await.unwrap()
    else {
        panic!("expected failed submit command");
    };
    failed_events
        .send(ResponsesEvent::RequestStarted(failed_request.request_id))
        .await
        .unwrap();
    failed_events
        .send(ResponsesEvent::Error {
            request_id: Some(failed_request.request_id),
            message: "HTTP error 400: context window exceeded maximum input tokens".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit {
        request: older_request,
        events: older_events,
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected older compact block");
    };
    let ResponsesCommand::Submit {
        request: newer_request,
        events: newer_events,
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected newer compact block");
    };
    older_events
        .send(ResponsesEvent::RequestStarted(older_request.request_id))
        .await
        .unwrap();
    newer_events
        .send(ResponsesEvent::RequestStarted(newer_request.request_id))
        .await
        .unwrap();
    older_events
        .send(ResponsesEvent::Frame {
            request_id: older_request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "older block summary"
            }),
        })
        .await
        .unwrap();
    newer_events
        .send(ResponsesEvent::Frame {
            request_id: newer_request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "newer block summary"
            }),
        })
        .await
        .unwrap();
    newer_events
        .send(ResponsesEvent::RequestCompleted(newer_request.request_id))
        .await
        .unwrap();
    older_events
        .send(ResponsesEvent::RequestCompleted(older_request.request_id))
        .await
        .unwrap();

    let ResponsesCommand::Submit {
        request: retry_request,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected retry submit command");
    };
    let retry_input = retry_request
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(retry_input.len(), 1);
    let retry_summary = message_input_text(&retry_input[0]);
    assert!(retry_summary.contains("older block summary"));
    assert!(retry_summary.contains("newer block summary"));
    assert!(retry_summary.contains("chronological block 1 of 2"));
}

#[tokio::test]
async fn context_policy_compaction_waits_for_retired_root_transport_completion() {
    let root = temp_root("context-policy-waits-for-root-completion");
    let mut config = test_config(root);
    add_small_context_model(&mut config);
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    skip_provider_changed_event(&runtime).await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "call a tool before compacting".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();

    runtime
        .commands
        .send(RuntimeCommand::SetModelSettings {
            settings: ModelSettings::new(
                "gpt-small",
                Some("xhigh".to_string()),
                Some("priority".to_string()),
            ),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::ModelSettingsChanged(settings) if settings.model == "gpt-small")
        })
        .await;

    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: FreeformToolCall {
                call_id: "call-missing".to_string(),
                name: "missing_tool".to_string(),
                input: "tool input that is durable".to_string(),
            },
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(
            event,
            RuntimeEvent::FreeformToolOutput(output)
                if output.call_id == "call-missing"
                    && output.output == "freeform tool execution is not implemented: missing_tool"
        )
    })
    .await;
    runtime
        .commands
        .send(RuntimeCommand::QueueSteering {
            text: "drain after deferred compaction".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::SteeringQueued(Some(text)) if text == "drain after deferred compaction")
        })
        .await;
    assert!(responses_rx.try_recv().is_err());

    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();
    let ResponsesCommand::Submit {
        request: compact_request,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected deferred compact submit command");
    };
    drop(compact_request);
}

#[tokio::test]
async fn context_policy_compaction_waits_for_root_context_window_error() {
    let root = temp_root("context-policy-waits-for-root-context-error");
    let mut config = test_config(root);
    add_small_context_model(&mut config);
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    skip_provider_changed_event(&runtime).await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "call a tool before context error compacting".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();

    runtime
        .commands
        .send(RuntimeCommand::SetModelSettings {
            settings: ModelSettings::new(
                "gpt-small",
                Some("xhigh".to_string()),
                Some("priority".to_string()),
            ),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::ModelSettingsChanged(settings) if settings.model == "gpt-small")
        })
        .await;

    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: FreeformToolCall {
                call_id: "call-missing".to_string(),
                name: "missing_tool".to_string(),
                input: "tool input that is durable".to_string(),
            },
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(
            event,
            RuntimeEvent::FreeformToolOutput(output)
                if output.call_id == "call-missing"
                    && output.output == "freeform tool execution is not implemented: missing_tool"
        )
    })
    .await;
    runtime
        .commands
        .send(RuntimeCommand::QueueSteering {
            text: "drain before context error".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
            matches!(event, RuntimeEvent::SteeringQueued(Some(text)) if text == "drain before context error")
        })
        .await;
    assert!(responses_rx.try_recv().is_err());

    events
        .send(ResponsesEvent::Error {
            request_id: Some(request.request_id),
            message: "HTTP error 400: context window exceeded maximum input tokens".to_string(),
        })
        .await
        .unwrap();
    let ResponsesCommand::Submit {
        request: older_request,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected older compact block");
    };
    let ResponsesCommand::Submit {
        request: newer_request,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected newer compact block");
    };
    drop((older_request, newer_request));
}

#[tokio::test]
async fn terminal_tool_model_output_is_colorless_while_transcript_output_keeps_ansi() {
    let root = temp_root("freeform-terminal-tool-color-output");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let mut config = test_config(root.clone());
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = terminal_tool_registry();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "run a colored command".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: FreeformToolCall {
                call_id: "call-exec-color".to_string(),
                name: TERMINAL_OPEN_TOOL_NAME.to_string(),
                input: "command: printf '\\033[31mred\\033[0m\\n'".to_string(),
            },
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit {
        request: continuation,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected continuation submit command");
    };
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let output = input[2]
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert!(output.contains("\nOutput:\nred\n"));
    assert!(!output.contains("\u{1b}[31m"));

    let summary = {
        let mut index = crate::sessions::SessionIndex::load(&root).unwrap();
        index
            .summary_by_id(&SessionId::new("harness-session"))
            .unwrap()
            .unwrap()
    };
    let records = SessionStore::new(&root).read_records(summary.path).unwrap();
    let output_record = records
        .iter()
        .find_map(|record| match &record.kind {
            SessionRecordKind::FreeformToolOutput(output)
                if output.call_id == "call-exec-color" =>
            {
                Some(output)
            }
            _ => None,
        })
        .expect("terminal tool output record");
    assert!(output_record.output.contains("\nOutput:\nred\n"));
    assert!(!output_record.output.contains("\u{1b}[31m"));
    assert!(
        output_record
            .display_output
            .as_deref()
            .is_some_and(|output| output.contains("\u{1b}[31mred\u{1b}[0m"))
    );
}

#[tokio::test]
async fn freeform_tool_input_deltas_are_assembled_for_execution() {
    let root = temp_root("freeform-tool-delta-continuation");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let mut config = test_config(root);
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = terminal_tool_registry();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "run from deltas".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolInputDelta {
            request_id: request.request_id,
            delta: crate::tools::FreeformToolInputDelta {
                item_id: "item-freeform".to_string(),
                call_id: Some("call-delta".to_string()),
                delta: "command: printf ".to_string(),
            },
        })
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolInputDelta {
            request_id: request.request_id,
            delta: crate::tools::FreeformToolInputDelta {
                item_id: "item-freeform".to_string(),
                call_id: Some("call-delta".to_string()),
                delta: "delta-ok".to_string(),
            },
        })
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: FreeformToolCall {
                call_id: "call-delta".to_string(),
                name: TERMINAL_OPEN_TOOL_NAME.to_string(),
                input: String::new(),
            },
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit {
        request: continuation,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected continuation submit command");
    };
    let body = continuation.body.as_object().unwrap();
    let input = body
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let call_item = input[1].as_object().unwrap();
    assert_eq!(
        call_item.get(&"input").and_then(JsonValueTrait::as_str),
        Some("command: printf delta-ok")
    );
    let output = input[2]
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert!(
        output.contains("\nProcess exited with code 0\nOutput:\ndelta-ok"),
        "{output}"
    );
}

#[tokio::test]
async fn terminal_write_targets_running_terminal_session() {
    let root = temp_root("terminal-write-tool-continuation");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let mut config = test_config(root.clone());
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = terminal_tool_registry();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "run interactive command".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: FreeformToolCall {
                call_id: "call-exec".to_string(),
                name: TERMINAL_OPEN_TOOL_NAME.to_string(),
                input: "command: printf ready; read line; printf \" got:$line\"".to_string(),
            },
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit {
        request: continuation,
        events: continuation_events,
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected continuation submit command");
    };
    let body = continuation.body.as_object().unwrap();
    let input = body
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let first_output = input[2]
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert!(first_output.contains("Terminal running with ID 1"));
    assert!(first_output.ends_with("\nready"));

    continuation_events
        .send(ResponsesEvent::RequestStarted(continuation.request_id))
        .await
        .unwrap();
    continuation_events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: continuation.request_id,
            call: FreeformToolCall {
                call_id: "call-stdin".to_string(),
                name: TERMINAL_WRITE_TOOL_NAME.to_string(),
                input: "terminal: 1\ninput:\nworld\n".to_string(),
            },
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit {
        request: final_request,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected final continuation submit command");
    };
    let body = final_request.body.as_object().unwrap();
    let input = body
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(input.len(), 5);
    let output_item = input[4].as_object().unwrap();
    assert_eq!(
        output_item.get(&"type").and_then(JsonValueTrait::as_str),
        Some("custom_tool_call_output")
    );
    assert_eq!(
        output_item.get(&"call_id").and_then(JsonValueTrait::as_str),
        Some("call-stdin")
    );
    let output = output_item
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert!(output.contains("Process exited with code 0"));
    assert!(output.contains("got:world"));
}

#[tokio::test]
async fn large_non_sed_terminal_output_is_summarized_before_continuation() {
    let root = temp_root("terminal-summary-continuation");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("large.txt"), large_terminal_text()).unwrap();
    let mut config = test_config(root);
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = terminal_tool_registry();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "read the large file".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial root submit").await;
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: FreeformToolCall {
                call_id: "call-large-terminal".to_string(),
                name: TERMINAL_OPEN_TOOL_NAME.to_string(),
                input: "command: cat large.txt".to_string(),
            },
        })
        .await
        .unwrap();

    let (summary_request, summary_events) =
        next_responses_submit(&responses_rx, "tool output summary submit").await;
    assert_eq!(
        summary_request
            .body
            .as_object()
            .unwrap()
            .get(&"model")
            .and_then(JsonValueTrait::as_str),
        Some("gpt-5.4")
    );
    assert_eq!(response_tool_names(&summary_request), Vec::<String>::new());
    let summary_input = summary_request
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(summary_input.len(), 1);

    summary_events
        .send(ResponsesEvent::RequestStarted(summary_request.request_id))
        .await
        .unwrap();
    summary_events
        .send(ResponsesEvent::Frame {
            request_id: summary_request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "large file contains repeated summary payload lines"
            }),
        })
        .await
        .unwrap();
    summary_events
        .send(ResponsesEvent::RequestCompleted(summary_request.request_id))
        .await
        .unwrap();

    let (continuation, _) = next_responses_submit(&responses_rx, "post-summary continuation").await;
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let output = input[2]
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert!(output.contains("terminal output summarized by gpt-5.5-low"));
    assert!(output.contains("large file contains repeated summary payload lines"));
    assert!(!output.contains("line-4999 summary payload"));
}

#[tokio::test]
async fn large_fff_output_is_bounded_before_continuation() {
    let root = temp_root("fff-bounded-continuation");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("large.txt"), large_terminal_text()).unwrap();
    let mut config = test_config(root);
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = NativeToolRegistry::codex();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "search the large file".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial fff submit").await;
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: FreeformToolCall {
                call_id: "call-large-fff".to_string(),
                name: INSPECT_TOOL_NAME.to_string(),
                input: "search summary".to_string(),
            },
        })
        .await
        .unwrap();

    let (continuation, _) = next_responses_submit(&responses_rx, "bounded fff continuation").await;
    assert_eq!(
        continuation
            .body
            .as_object()
            .unwrap()
            .get(&"model")
            .and_then(JsonValueTrait::as_str),
        Some("gpt-test")
    );
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let output = input[2]
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert!(output.contains("line-0000 summary payload"));
    assert!(output.contains("line-0049 summary payload"));
    assert!(!output.contains("terminal output summarized by gpt-5.5-low"));
    assert!(!output.contains("line-0050 summary payload"));
    assert!(!output.contains("line-4999 summary payload"));
}

#[test]
fn fff_model_output_cap_keeps_search_output_bounded() {
    let output = cap_model_output_for_estimated_tokens(large_terminal_text(), 100, "fff");

    assert!(crate::compact::estimate_text_tokens(&output) <= 100);
    assert!(output.contains("fff output truncated before summarization"));
    assert!(output.contains("line-0000 summary payload"));
    assert!(!output.contains("line-4999 summary payload"));
}

#[test]
fn fff_file_path_constraint_limits_results_to_exact_file() {
    let root = temp_root("fff-exact-file");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/lib.rs"), "fn needle() {}\n").unwrap();
    fs::write(project.join("src/main.rs"), "fn needle() {}\n").unwrap();

    let output =
        execute_search_command(project.to_str().unwrap(), "search needle src/lib.rs").output;

    assert!(output.model_output.contains("src/lib.rs\n1 fn needle() {}"));
    assert!(!output.model_output.contains("src/main.rs"));
}

#[test]
fn fff_total_match_cap_adds_refinement_notice() {
    let root = temp_root("fff-total-cap");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    for file_index in 0..3 {
        let mut content = String::new();
        for line_index in 0..50 {
            writeln!(
                content,
                "file-{file_index} line-{line_index:04} needle payload"
            )
            .unwrap();
        }
        fs::write(project.join(format!("src/file_{file_index}.txt")), content).unwrap();
    }

    let output = execute_search_command(project.to_str().unwrap(), "search needle src").output;

    assert!(
        output
            .model_output
            .contains("file-0 line-0000 needle payload")
    );
    assert!(
        output
            .model_output
            .contains("file-1 line-0049 needle payload")
    );
    assert!(
        !output
            .model_output
            .contains("file-2 line-0000 needle payload")
    );
    assert!(output.model_output.contains(
        "[fff output truncated: showing first 100 of 150 matches; refine the query or path constraint]"
    ));
}

#[tokio::test]
async fn explicit_sed_range_terminal_output_bypasses_summarizer() {
    let root = temp_root("terminal-summary-sed-bypass");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("large.txt"), large_terminal_text()).unwrap();
    let mut config = test_config(root);
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = terminal_tool_registry();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "read the ranged file".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial root submit").await;
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: FreeformToolCall {
                call_id: "call-large-sed".to_string(),
                name: TERMINAL_OPEN_TOOL_NAME.to_string(),
                input: "command: sed -n '1,5000p' large.txt".to_string(),
            },
        })
        .await
        .unwrap();

    let (continuation, _) = next_responses_submit(&responses_rx, "direct sed continuation").await;
    assert_ne!(
        continuation
            .body
            .as_object()
            .unwrap()
            .get(&"model")
            .and_then(JsonValueTrait::as_str),
        Some("gpt-5.4")
    );
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let output = input[2]
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert!(output.contains("line-4999 summary payload"));
    assert!(!output.contains("terminal output summarized by gpt-5.5-low"));
}

#[tokio::test]
async fn retryable_tool_output_summary_error_retries_before_continuation() {
    let root = temp_root("terminal-summary-retry");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("large.txt"), large_terminal_text()).unwrap();
    let mut config = test_config(root);
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = terminal_tool_registry();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "read the large file with retry".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial root submit").await;
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: FreeformToolCall {
                call_id: "call-large-retry".to_string(),
                name: TERMINAL_OPEN_TOOL_NAME.to_string(),
                input: "command: cat large.txt".to_string(),
            },
        })
        .await
        .unwrap();

    let (first_summary, first_summary_events) =
        next_responses_submit(&responses_rx, "first summary submit").await;
    first_summary_events
        .send(ResponsesEvent::Error {
            request_id: Some(first_summary.request_id),
            message: "websocket closed by server before response.completed".to_string(),
        })
        .await
        .unwrap();

    let (retry_summary, retry_summary_events) =
        next_responses_submit(&responses_rx, "retry summary submit").await;
    assert_ne!(retry_summary.request_id, first_summary.request_id);
    assert_eq!(
        retry_summary
            .body
            .as_object()
            .unwrap()
            .get(&"model")
            .and_then(JsonValueTrait::as_str),
        Some("gpt-5.4")
    );

    retry_summary_events
        .send(ResponsesEvent::RequestStarted(retry_summary.request_id))
        .await
        .unwrap();
    retry_summary_events
        .send(ResponsesEvent::Frame {
            request_id: retry_summary.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "summary after retry"
            }),
        })
        .await
        .unwrap();
    retry_summary_events
        .send(ResponsesEvent::RequestCompleted(retry_summary.request_id))
        .await
        .unwrap();

    let (continuation, _) = next_responses_submit(&responses_rx, "post-retry continuation").await;
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let output = input[2]
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert!(output.contains("summary after retry"));
    assert!(!output.contains("terminal output summary failed"));
}

#[tokio::test]
async fn apply_patch_tool_call_executes_and_submits_continuation() {
    let root = temp_root("apply-patch-tool-continuation");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let mut config = test_config(root.clone());
    config.cwd = project.to_str().unwrap().to_string();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "create a file".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
            .send(ResponsesEvent::FreeformToolCall {
                request_id: request.request_id,
                call: crate::tools::FreeformToolCall {
                    call_id: "call-apply-patch".to_string(),
                    name: APPLY_PATCH_TOOL_NAME.to_string(),
                    input: "*** Begin Patch\n*** Add File: nested/created.txt\n+hello from patch\n*** End Patch\n"
                        .to_string(),
                },
            })
            .await
            .unwrap();

    let ResponsesCommand::Submit {
        request: continuation,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected continuation submit command");
    };
    assert_eq!(
        fs::read_to_string(project.join("nested/created.txt")).unwrap(),
        "hello from patch\n"
    );

    let body = continuation.body.as_object().unwrap();
    let input = body
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(input.len(), 3);
    let output_item = input[2].as_object().unwrap();
    assert_eq!(
        output_item.get(&"type").and_then(JsonValueTrait::as_str),
        Some("custom_tool_call_output")
    );
    assert_eq!(
        output_item.get(&"call_id").and_then(JsonValueTrait::as_str),
        Some("call-apply-patch")
    );
    assert_eq!(
        output_item.get(&"output").and_then(JsonValueTrait::as_str),
        Some("Success. Updated the following files:\nA nested/created.txt\n")
    );

    let summary = {
        let mut index = crate::sessions::SessionIndex::load(&root).unwrap();
        index
            .summary_by_id(&SessionId::new("harness-session"))
            .unwrap()
            .unwrap()
    };
    let records = SessionStore::new(&root).read_records(summary.path).unwrap();
    assert!(matches!(
        records[3].kind,
        SessionRecordKind::FreeformToolCall(_)
    ));
    assert!(matches!(
        records[4].kind,
        SessionRecordKind::FreeformToolOutput(_)
    ));
}

#[tokio::test]
async fn edit_file_anchor_replace_mutates_immediately() {
    let root = temp_root("edit-file-anchor-replace");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("target.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    let text = fs::read_to_string(project.join("target.txt")).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    let start = format_line_anchor(2, edit_line_hash(lines[1]));
    let end = format_line_anchor(3, edit_line_hash(lines[2]));
    let mut config = test_config(root);
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = NativeToolRegistry::codex();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "edit target".to_string(),
        })
        .await
        .unwrap();

    let (root_request, root_events) =
        next_responses_submit(&responses_rx, "initial root submit").await;
    root_events
        .send(ResponsesEvent::RequestStarted(root_request.request_id))
        .await
        .unwrap();
    root_events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: root_request.request_id,
            call: FreeformToolCall {
                call_id: "call-edit".to_string(),
                name: EDIT_FILE_TOOL_NAME.to_string(),
                input: format!("§ Edit target.txt\n§ Replace {start} {end}\nsecond\nthird\n"),
            },
        })
        .await
        .unwrap();

    let (continuation, _) = next_responses_submit(&responses_rx, "edit continuation submit").await;
    assert_eq!(
        fs::read_to_string(project.join("target.txt")).unwrap(),
        "one\nsecond\nthird\nfour\n"
    );
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let output = input[2]
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert_eq!(output, "ok");
}

#[test]
fn edit_file_success_output_has_no_transcript_diff_payload() {
    let root = temp_root("edit-file-display-diff");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("target.txt"), "one\ntwo\nthree\n").unwrap();
    let text = fs::read_to_string(project.join("target.txt")).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    let start = format_line_anchor(2, edit_line_hash(lines[1]));
    let end = format_line_anchor(2, edit_line_hash(lines[1]));

    let output = execute_edit_file_tool(
        project.to_str().unwrap(),
        &format!("§ Edit target.txt\n§ Replace {start} {end}\nsecond\n"),
    );

    assert_eq!(output.model_output, "ok");
    assert_eq!(output.display_output, "ok");

    let record = freeform_tool_output_record("call-edit".to_string(), output);
    assert_eq!(record.output, "ok");
    assert_eq!(record.display_output, None);
}
#[tokio::test]
async fn edit_file_anchor_insert_and_remove_file() {
    let root = temp_root("edit-file-anchor-insert-remove");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("target.txt"), "one\ntwo\n").unwrap();
    let text = fs::read_to_string(project.join("target.txt")).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    let anchor = format_line_anchor(1, edit_line_hash(lines[0]));
    let mut config = test_config(root);
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = NativeToolRegistry::codex();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "insert then remove target".to_string(),
        })
        .await
        .unwrap();

    let (root_request, root_events) =
        next_responses_submit(&responses_rx, "initial insert root submit").await;
    root_events
        .send(ResponsesEvent::RequestStarted(root_request.request_id))
        .await
        .unwrap();
    root_events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: root_request.request_id,
            call: FreeformToolCall {
                call_id: "call-insert".to_string(),
                name: EDIT_FILE_TOOL_NAME.to_string(),
                input: format!("§ Edit target.txt\n§ After {anchor}\nmiddle\n"),
            },
        })
        .await
        .unwrap();

    let (insert_continuation, insert_events) =
        next_responses_submit(&responses_rx, "insert continuation submit").await;
    assert_eq!(
        fs::read_to_string(project.join("target.txt")).unwrap(),
        "one\nmiddle\ntwo\n"
    );
    let insert_input = insert_continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let insert_output = insert_input[2]
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert_eq!(insert_output, "ok");
    insert_events
        .send(ResponsesEvent::RequestStarted(
            insert_continuation.request_id,
        ))
        .await
        .unwrap();
    insert_events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: insert_continuation.request_id,
            call: FreeformToolCall {
                call_id: "call-remove".to_string(),
                name: EDIT_FILE_TOOL_NAME.to_string(),
                input: "§ Remove target.txt".to_string(),
            },
        })
        .await
        .unwrap();

    let (remove_continuation, _) =
        next_responses_submit(&responses_rx, "remove continuation submit").await;
    assert!(!project.join("target.txt").exists());
    let remove_input = remove_continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let remove_output = remove_input
        .last()
        .unwrap()
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert_eq!(remove_output, "ok");
}

#[test]
fn edit_file_tool_applies_multiple_segments_and_files() {
    let root = temp_root("edit-file-multi-segment-file");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    fs::write(project.join("b.txt"), "remove\nkeep\n").unwrap();
    let a_text = fs::read_to_string(project.join("a.txt")).unwrap();
    let a_lines = a_text.lines().collect::<Vec<_>>();
    let a_two = format_line_anchor(2, edit_line_hash(a_lines[1]));
    let a_three = format_line_anchor(3, edit_line_hash(a_lines[2]));
    let b_text = fs::read_to_string(project.join("b.txt")).unwrap();
    let b_lines = b_text.lines().collect::<Vec<_>>();
    let b_remove = format_line_anchor(1, edit_line_hash(b_lines[0]));
    let request = parse_edit_file_tool_input(&format!(
        "§ Edit a.txt\n§ Replace {a_two} {a_two}\nsecond\n§ After {a_three}\nfour\n§ Edit b.txt\n§ Delete {b_remove} {b_remove}\n§ Add c.txt\ncreated\n"
    ))
    .unwrap();

    let output = apply_edit_file_tool_request(project.to_str().unwrap(), &request).unwrap();

    assert_eq!(
        fs::read_to_string(project.join("a.txt")).unwrap(),
        "one\nsecond\nthree\nfour\n"
    );
    assert_eq!(fs::read_to_string(project.join("b.txt")).unwrap(), "keep\n");
    assert_eq!(
        fs::read_to_string(project.join("c.txt")).unwrap(),
        "created\n"
    );
    assert_eq!(output, "ok");
}

#[test]
fn edit_file_tool_reports_partial_success_and_all_operation_errors() {
    let root = temp_root("edit-file-partial-success-errors");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("a.txt"), "one\ntwo\n").unwrap();
    fs::write(project.join("b.txt"), "alpha\nbeta\n").unwrap();

    let a_text = fs::read_to_string(project.join("a.txt")).unwrap();
    let a_lines = a_text.lines().collect::<Vec<_>>();
    let a_two = format_line_anchor(2, edit_line_hash(a_lines[1]));

    let b_text = fs::read_to_string(project.join("b.txt")).unwrap();
    let b_lines = b_text.lines().collect::<Vec<_>>();
    let stale_b_one = format_line_anchor(1, edit_line_hash(b_lines[0]).wrapping_add(1));

    let output = execute_edit_file_tool(
        project.to_str().unwrap(),
        &format!(
            "§ Edit a.txt\n§ Replace {a_two} {a_two}\nsecond\n§ Edit b.txt\n§ Replace {stale_b_one} {stale_b_one}\nALPHA\n§ Add c.txt\ncreated\n§ Remove missing.txt\n"
        ),
    );

    assert_eq!(
        fs::read_to_string(project.join("a.txt")).unwrap(),
        "one\nsecond\n"
    );
    assert_eq!(
        fs::read_to_string(project.join("b.txt")).unwrap(),
        "alpha\nbeta\n"
    );
    assert_eq!(
        fs::read_to_string(project.join("c.txt")).unwrap(),
        "created\n"
    );

    assert!(output.model_output.starts_with("edit errors\n"));
    assert!(output.model_output.contains("2 b.txt stale anchor"));
    assert!(output.model_output.contains("4 failed to read "));
    assert!(output.model_output.contains("missing.txt"));
    assert!(!output.model_output.contains("changed:"));
    assert_eq!(output.display_output, output.model_output);
}
#[test]
fn edit_file_rejects_git_paths_for_all_operations() {
    let root = temp_root("edit-file-git-sandbox");
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(project.join("target.txt"), "one\ntwo\n").unwrap();
    let text = fs::read_to_string(project.join("target.txt")).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    let anchor = format_line_anchor(1, edit_line_hash(lines[0]));
    let cwd = project.to_str().unwrap();

    let edit_output = execute_edit_file_tool(
        cwd,
        &format!("§ Edit .git/HEAD\n§ Replace {anchor} {anchor}\ntampered\n"),
    );
    assert!(
        edit_output.model_output.contains(".git` are sandboxed"),
        "Edit into .git should be rejected: {}",
        edit_output.model_output
    );
    assert_eq!(
        fs::read_to_string(project.join(".git/HEAD")).unwrap(),
        "ref: refs/heads/main\n",
        ".git/HEAD must remain unmodified"
    );

    let add_output = execute_edit_file_tool(cwd, "§ Add .git/config\n[tampered]\n");
    assert!(
        add_output.model_output.contains(".git` are sandboxed"),
        "Add into .git should be rejected: {}",
        add_output.model_output
    );

    let remove_output = execute_edit_file_tool(cwd, "§ Remove .git/HEAD\n");
    assert!(
        remove_output.model_output.contains(".git` are sandboxed"),
        "Remove of .git should be rejected: {}",
        remove_output.model_output
    );
    assert!(project.join(".git/HEAD").exists());

    let move_into_output = execute_edit_file_tool(cwd, "§ Move target.txt\n§ To .git/HEAD\n");
    assert!(
        move_into_output
            .model_output
            .contains(".git` are sandboxed"),
        "Move into .git should be rejected: {}",
        move_into_output.model_output
    );
    assert!(
        project.join("target.txt").exists(),
        "source must not be moved"
    );
    assert!(project.join(".git/HEAD").exists());

    let move_from_output = execute_edit_file_tool(cwd, "§ Move .git/HEAD\n§ To relocated\n");
    assert!(
        move_from_output
            .model_output
            .contains(".git` are sandboxed"),
        "Move out of .git should be rejected: {}",
        move_from_output.model_output
    );
    assert!(project.join(".git/HEAD").exists());
}
#[tokio::test]
async fn locate_tool_returns_completed_frame_output_without_delta() {
    let root = temp_root("locate-tool-completed-frame");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("src/lib.rs"),
        "pub fn destroy_data_device() {}\n",
    )
    .unwrap();

    let mut config = test_config(root);
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = NativeToolRegistry::codex();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "locate destroy_data_device".to_string(),
        })
        .await
        .unwrap();

    let (root_request, root_events) =
        next_responses_submit(&responses_rx, "initial locate completed-frame root submit").await;
    root_events
        .send(ResponsesEvent::RequestStarted(root_request.request_id))
        .await
        .unwrap();
    root_events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: root_request.request_id,
            call: FreeformToolCall {
                call_id: "call-locate".to_string(),
                name: LOCATE_TOOL_NAME.to_string(),
                input: "destroy_data_device definition".to_string(),
            },
        })
        .await
        .unwrap();

    let (worker_request, worker_events) =
        next_responses_submit(&responses_rx, "locate completed-frame worker submit").await;
    worker_events
        .send(ResponsesEvent::Frame {
            request_id: worker_request.request_id,
            body: json!({
                "type": "response.completed",
                "response": {
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "src/lib.rs:1\npub fn destroy_data_device() {}"
                        }]
                    }]
                }
            }),
        })
        .await
        .unwrap();
    worker_events
        .send(ResponsesEvent::RequestCompleted(worker_request.request_id))
        .await
        .unwrap();

    let (continuation, _) =
        next_responses_submit(&responses_rx, "locate completed-frame continuation submit").await;
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let output = input
        .last()
        .unwrap()
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();

    assert!(output.contains("src/lib.rs:1"));
    assert!(output.contains("destroy_data_device"));
    assert!(!output.contains("locator returned no output"));
}

#[tokio::test]
async fn locate_worker_executes_private_inspect_call_before_returning_context() {
    let root = temp_root("locate-tool-private-inspect");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/lib.rs"), "pub fn private_target() {}\n").unwrap();
    let mut config = test_config(root);
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = NativeToolRegistry::codex();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "locate private inspect".to_string(),
        })
        .await
        .unwrap();

    let (root_request, root_events) =
        next_responses_submit(&responses_rx, "initial private locate root submit").await;
    root_events
        .send(ResponsesEvent::RequestStarted(root_request.request_id))
        .await
        .unwrap();
    root_events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: root_request.request_id,
            call: FreeformToolCall {
                call_id: "call-locate-private-inspect".to_string(),
                name: LOCATE_TOOL_NAME.to_string(),
                input: "need context for private inspect search".to_string(),
            },
        })
        .await
        .unwrap();

    let (worker_request, worker_events) =
        next_responses_submit(&responses_rx, "private locate worker submit").await;
    assert_eq!(
        response_tool_names(&worker_request),
        vec![INSPECT_TOOL_NAME.to_string()]
    );
    worker_events
        .send(ResponsesEvent::RequestStarted(worker_request.request_id))
        .await
        .unwrap();
    worker_events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: worker_request.request_id,
            call: FreeformToolCall {
                call_id: "call-private-inspect".to_string(),
                name: INSPECT_TOOL_NAME.to_string(),
                input: "pwd\nsearch private_target src\n".to_string(),
            },
        })
        .await
        .unwrap();

    let (second_worker_request, second_worker_events) =
        next_responses_submit(&responses_rx, "private locate worker continuation").await;
    let second_input = second_worker_request
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert!(second_input.iter().any(|item| {
        item.as_object()
            .unwrap()
            .get(&"type")
            .and_then(JsonValueTrait::as_str)
            == Some("custom_tool_call")
    }));
    assert!(second_input.iter().any(|item| {
        item.as_object()
            .unwrap()
            .get(&"type")
            .and_then(JsonValueTrait::as_str)
            == Some("custom_tool_call_output")
            && item
                .as_object()
                .unwrap()
                .get(&"output")
                .and_then(JsonValueTrait::as_str)
                .is_some_and(|output| {
                    output.contains(project.to_str().unwrap()) && output.contains("private_target")
                })
    }));

    second_worker_events
        .send(ResponsesEvent::RequestStarted(
            second_worker_request.request_id,
        ))
        .await
        .unwrap();
    second_worker_events
        .send(ResponsesEvent::Frame {
            request_id: second_worker_request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "- src/lib.rs:1 — private_target — function definition"
            }),
        })
        .await
        .unwrap();
    second_worker_events
        .send(ResponsesEvent::RequestCompleted(
            second_worker_request.request_id,
        ))
        .await
        .unwrap();

    let (continuation, _) =
        next_responses_submit(&responses_rx, "private locate root continuation").await;
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let output = input
        .last()
        .unwrap()
        .as_object()
        .unwrap()
        .get(&"output")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    assert!(output.contains("private_target"));
}

#[test]
fn edit_file_tool_input_parser_reads_anchor_replace_and_file_ops() {
    let request =
        parse_edit_file_tool_input("§ Edit src/lib.rs\n§ Replace 24 bucket 26 year\nnew line\n")
            .unwrap();
    assert_eq!(
        request,
        EditFileToolRequest {
            operations: vec![EditFileOperation::Edit {
                path: "src/lib.rs".to_string(),
                segments: vec![EditSegment::Replace {
                    start: LineAnchor {
                        line_number: 24,
                        hash: 0xa3,
                    },
                    end: LineAnchor {
                        line_number: 26,
                        hash: 0x7c,
                    },
                    body: "new line\n".to_string(),
                }],
            }],
        }
    );
    assert_eq!(
        parse_edit_file_tool_input("§ Edit src/lib.rs\n§ Delete 24 bucket 26 year\n").unwrap(),
        EditFileToolRequest {
            operations: vec![EditFileOperation::Edit {
                path: "src/lib.rs".to_string(),
                segments: vec![EditSegment::Delete {
                    start: LineAnchor {
                        line_number: 24,
                        hash: 0xa3,
                    },
                    end: LineAnchor {
                        line_number: 26,
                        hash: 0x7c,
                    },
                }],
            }],
        }
    );

    assert_eq!(
        parse_edit_file_tool_input("§ Remove src/lib.rs").unwrap(),
        EditFileToolRequest {
            operations: vec![EditFileOperation::Remove {
                path: "src/lib.rs".to_string(),
            }],
        }
    );

    assert_eq!(
        parse_edit_file_tool_input("§ Move old.rs\n§ To new.rs\n").unwrap(),
        EditFileToolRequest {
            operations: vec![EditFileOperation::Move {
                from: "old.rs".to_string(),
                to: "new.rs".to_string(),
            }],
        }
    );
}

#[test]
fn edit_file_tool_input_parser_rejects_invalid_anchor_format() {
    let error =
        parse_edit_file_tool_input("§ Edit src/lib.rs\n§ Replace 24 unknown 26 year\nnew line\n")
            .unwrap_err();
    assert_eq!(
        error,
        "failed to parse `edit_file` input: anchor word is not in the vocabulary"
    );
}

#[test]
fn edit_file_tool_input_parser_rejects_with_delimiters() {
    let error =
        parse_edit_file_tool_input("*** Edit src/lib.rs\n*** Replace 24a3 267c\n*** With body\n")
            .unwrap_err();

    assert_eq!(
        error,
        "failed to parse `edit_file` input: `***` patch delimiters are not supported"
    );
}
#[test]
fn edit_file_tool_input_parser_rejects_with_header() {
    let error = parse_edit_file_tool_input(
        "§ Edit src/lib.rs\n§ Replace 308 function 310 atom\n\
         if branch == 2 || branch == 3 {\n\
             return self.decode_ranked_history_group(input, 16);\n\
         }\n\
         § With 308 function 312 atom\n\
         if branch == 2 || branch == \n",
    )
    .unwrap_err();

    assert_eq!(
        error,
        "failed to parse `edit_file` input: unsupported edit header `§ With 308 function 312 atom`"
    );
}
#[test]
fn edit_file_tool_input_parser_requires_escaped_literal_section_symbols() {
    let request = parse_edit_file_tool_input("§ Add notes.txt\nprefix \\§ literal\n").unwrap();
    assert_eq!(
        request,
        EditFileToolRequest {
            operations: vec![EditFileOperation::Add {
                path: "notes.txt".to_string(),
                body: "prefix § literal\n".to_string(),
            }],
        }
    );

    let error = parse_edit_file_tool_input("§ Add notes.txt\nprefix § literal\n").unwrap_err();
    assert_eq!(
        error,
        "failed to parse `edit_file` input: literal `§` in body must be escaped as `\\§`"
    );
}

#[test]
fn inspect_read_range_parser_reads_counted_and_inclusive_ranges() {
    assert_eq!(
        parse_inspect_read_range("120+80").unwrap(),
        InspectReadRange {
            start_line: 120,
            line_count: 80,
        }
    );
    assert_eq!(
        parse_inspect_read_range("240-280").unwrap(),
        InspectReadRange {
            start_line: 240,
            line_count: 41,
        }
    );
}

#[test]
fn inspect_read_range_parser_rejects_invalid_ranges() {
    let error = parse_inspect_read_range("0+10").unwrap_err();
    assert_eq!(
        error,
        "failed to parse `inspect` input: range start must be a positive integer"
    );

    let error = parse_inspect_read_range("10-1").unwrap_err();
    assert_eq!(
        error,
        "failed to parse `inspect` input: range end must be >= start"
    );
}
#[test]
fn inspect_check_parser_accepts_packages_and_target_selectors() {
    assert_eq!(
        parse_cargo_check_command("check harness-core --lib --all-targets").unwrap(),
        vec![
            "check".to_string(),
            "--locked".to_string(),
            "-p".to_string(),
            "harness-core".to_string(),
            "--lib".to_string(),
            "--all-targets".to_string(),
        ]
    );
    assert_eq!(
        parse_cargo_check_command("check").unwrap(),
        vec!["check".to_string(), "--locked".to_string()]
    );

    let error = parse_cargo_check_command("check --message-format=short").unwrap_err();
    assert_eq!(
        error,
        "failed to parse `inspect` check input: expected package names, --lib, or --all-targets"
    );
}

#[test]
fn inspect_check_non_rust_failure_is_compact() {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg("exit 101")
        .status()
        .unwrap();
    let output = format_cargo_check_output(
        status,
        "",
        "Updating crates.io index\n     Locking 8 packages to latest compatible versions\n",
    );

    assert_eq!(output, "cargo check failed 101\n");
}
#[test]
fn inspect_check_rust_errors_are_grouped_by_path() {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg("exit 101")
        .status()
        .unwrap();
    let output = format_cargo_check_output(
        status,
        "",
        "error[E0308]: mismatched types\n --> src/lib.rs:10:5\nerror[E0425]: cannot find value `missing` in this scope\n --> src/lib.rs:20:9\nerror: unexpected closing delimiter: `}`\n --> src/main.rs:55:1\n",
    );

    assert_eq!(
        output,
        "E0 err lineposition\nsrc/lib.rs\n308 mismatched types 10:5\n425 cannot find value `missing` in this scope 20:9\nsrc/main.rs\n0 unexpected closing delimiter: `}` 55:1\n"
    );
}

#[test]
fn inspect_check_rust_unclosed_delimiter_error_is_simplified() {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg("exit 101")
        .status()
        .unwrap();
    let output = format_cargo_check_output(
        status,
        "",
        "    Checking jay-compositor v1.11.0 (/var/home/me/jay)\nerror: this file contains an unclosed delimiter\n   --> src/globals.rs:740:3\n    |\n371 | impl GlobalRecord {\n    |                   - unclosed delimiter\n...\n398 |         match self.kind {\n    |                         - this delimiter might not be properly closed...\n...\n454 |     }\n    |     - ...as it matches this but it has different indentation\n...\n740 | }\n    |  ^\n\nerror: could not compile `jay-compositor` (lib) due to 1 previous error\n",
    );

    assert_eq!(
        output,
        "E0 err lineposition\nsrc/globals.rs\n0 this file contains an unclosed delimiter 740:3\n"
    );
}
#[test]
fn inspect_test_output_summarizes_suites_and_retains_failures() {
    let success = std::process::Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .status()
        .unwrap();
    let output = format_cargo_test_output(
        success,
        "test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 4 filtered out; finished in 0.01s\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n",
        "",
    );
    assert_eq!(
        output,
        "ok: 3 passed; 0 failed; 1 ignored; 0 measured; 4 filtered out\n"
    );

    let failure = std::process::Command::new("sh")
        .arg("-c")
        .arg("exit 101")
        .status()
        .unwrap();
    let output = format_cargo_test_output(
        failure,
        "running 1 test\ntest module::fails ... FAILED\n\nfailures:\n\n---- module::fails stdout ----\nthread 'module::fails' panicked at src/lib.rs:12:5:\nassertion failed: false\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\nfailures:\n    module::fails\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n",
        "",
    );
    assert_eq!(
        output,
        "Test failures\nmodule::fails\nthread 'module::fails' panicked at src/lib.rs:12:5:\nassertion failed: false\nFAILED: 0 passed; 1 failed; 0 ignored; 0 measured; 3 filtered out\n"
    );
}

#[test]
fn inspect_test_output_retains_stack_overflow_panics_from_stderr() {
    let failure = std::process::Command::new("sh")
        .arg("-c")
        .arg("kill -ABRT $$")
        .status()
        .unwrap();
    let output = format_cargo_test_output(
        failure,
        "running 1 test\n",
        "thread 'executes_workload' (176968) has overflowed its stack\nfatal runtime error: stack overflow, aborting\nerror: test failed, to rerun pass `-p interp --test workload_tests`\n\nCaused by:\n  process didn't exit successfully (signal: 6, SIGABRT: process abort signal)\n",
    );

    assert_eq!(
        output,
        "Test failures\nthread 'executes_workload' (176968) has overflowed its stack\nfatal runtime error: stack overflow, aborting\ncargo test failed terminated by signal\n"
    );
}
#[test]
fn inspect_test_command_runs_full_suites_and_multiple_exact_filters() {
    let project = temp_root("inspect-cargo-test");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"inspect-cargo-test\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        project.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\nversion = 4\n\n[[package]]\nname = \"inspect-cargo-test\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        project.join("src/lib.rs"),
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn first() {}\n\n    #[test]\n    fn second() {}\n}\n",
    )
    .unwrap();
    let cwd = project.to_str().unwrap();

    let full_suite = execute_cargo_test_command(cwd, "test --lib");
    assert!(full_suite.succeeded);
    assert_eq!(
        full_suite.output.model_output,
        "ok: 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
    );

    let filtered =
        execute_cargo_test_command(cwd, "test --lib tests::first tests::second -- --exact");
    assert!(filtered.succeeded);
    assert_eq!(
        filtered.output.model_output,
        "filter tests::first\nok: 1 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out\nfilter tests::second\nok: 1 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out\n"
    );
}

#[test]
fn inspect_read_formats_plain_output() {
    let text = "one\ntwo\nthree\nfour\n";
    let lines = text.lines().collect::<Vec<_>>();
    let anchor_2 = format_line_anchor(2, edit_line_hash(lines[1]));
    let anchor_3 = format_line_anchor(3, edit_line_hash(lines[2]));
    let output = format_inspect_read_output(
        &InspectReadOutputRequest {
            path: "target.txt".to_string(),
            start_line: 2,
            line_count: 2,
        },
        text,
    );
    assert_eq!(
        output,
        format!("{anchor_2}two\n{anchor_3}three\nnext: 4+2\n")
    );
}

#[tokio::test]
async fn inspect_tool_call_executes_and_submits_continuation() {
    let root = temp_root("inspect-tool-continuation");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("target.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    let mut config = test_config(root.clone());
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = terminal_tool_registry();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "read target".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial inspect submit").await;
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: crate::tools::FreeformToolCall {
                call_id: "call-inspect".to_string(),
                name: INSPECT_TOOL_NAME.to_string(),
                input: "read target.txt\n2+2\n".to_string(),
            },
        })
        .await
        .unwrap();

    let (continuation, _) =
        next_responses_submit(&responses_rx, "inspect continuation submit").await;
    let input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let output_item = input[2].as_object().unwrap();
    assert_eq!(
        output_item.get(&"type").and_then(JsonValueTrait::as_str),
        Some("custom_tool_call_output")
    );
    let lines = ["one", "two", "three", "four"];
    let expected = format!(
        "{}two\n{}three\nnext: 4+2\n",
        format_line_anchor(2, edit_line_hash(lines[1])),
        format_line_anchor(3, edit_line_hash(lines[2])),
    );
    assert_eq!(
        output_item.get(&"output").and_then(JsonValueTrait::as_str),
        Some(expected.as_str())
    );
}

#[test]
fn inspect_input_parser_reads_mixed_jobs() {
    let request = parse_inspect_input(
        "read Cargo.toml\n1+2\n3-4\npwd\nbyte-search probe 89504e47\nstrings probe PNG\nelf probe symbols main\nsearch \"foo|bar\" src tests\nwhich crg\ntest --lib first second\nps new_harness\n",
    )
    .unwrap();
    assert_eq!(
        request.jobs,
        vec![
            InspectJob::Read(InspectReadJob {
                path: "Cargo.toml".to_string(),
                ranges: vec![
                    InspectReadRange {
                        start_line: 1,
                        line_count: 2,
                    },
                    InspectReadRange {
                        start_line: 3,
                        line_count: 2,
                    },
                ],
            }),
            InspectJob::Command(InspectCommandJob {
                command: "pwd".to_string(),
            }),
            InspectJob::Command(InspectCommandJob {
                command: "byte-search probe 89504e47".to_string(),
            }),
            InspectJob::Command(InspectCommandJob {
                command: "strings probe PNG".to_string(),
            }),
            InspectJob::Command(InspectCommandJob {
                command: "elf probe symbols main".to_string(),
            }),
            InspectJob::Command(InspectCommandJob {
                command: "search \"foo|bar\" src tests".to_string(),
            }),
            InspectJob::Command(InspectCommandJob {
                command: "which crg".to_string(),
            }),
            InspectJob::Command(InspectCommandJob {
                command: "test --lib first second".to_string(),
            }),
            InspectJob::Command(InspectCommandJob {
                command: "ps new_harness".to_string(),
            }),
        ]
    );
}

#[test]
fn inspect_read_supports_quoted_paths() {
    let root = temp_root("inspect-read-quoted-path");
    fs::write(root.join("with spaces.txt"), "first\nsecond\n").unwrap();

    let request = parse_inspect_input("read \"with spaces.txt\" 2+1").unwrap();
    let output = execute_inspect_job(root.to_str().unwrap(), &request.jobs[0]);

    assert!(output.model_output.ends_with("second\n"));
}

#[test]
fn inspect_list_reports_navigation_facts_without_stat_metadata() {
    use std::os::unix::fs::symlink;

    let root = temp_root("inspect-list");
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::write(root.join("source.rs"), "one\ntwo").unwrap();
    fs::write(root.join("binary"), [0_u8; 1_500]).unwrap();
    symlink("empty", root.join("current")).unwrap();

    let execution = execute_list_command(root.to_str().unwrap(), "list");
    assert!(execution.succeeded);
    assert_eq!(
        execution.output.model_output,
        "binary 2 KB\ncurrent/ -> empty/\nempty/\nsource.rs 2 lines\n"
    );
    assert!(!execution.output.model_output.contains("modified:"));
    assert!(!execution.output.model_output.contains("permissions:"));
    let exact = execute_list_command(root.to_str().unwrap(), "list --exact");
    assert!(exact.output.model_output.contains("binary 1500 bytes\n"));
    assert!(
        exact
            .output
            .model_output
            .contains("source.rs 2 lines 7 bytes\n")
    );
}

#[test]
fn inspect_list_depth_is_bounded_and_does_not_follow_directory_symlinks() {
    use std::os::unix::fs::symlink;

    let root = temp_root("inspect-list-depth");
    fs::create_dir_all(root.join("nested/deeper")).unwrap();
    fs::write(root.join("nested/deeper/file.txt"), "content\n").unwrap();
    symlink("nested", root.join("alias")).unwrap();

    let shallow = execute_list_command(root.to_str().unwrap(), "list");
    assert_eq!(shallow.output.model_output, "alias/ -> nested/\nnested/\n");

    let recursive = execute_list_command(root.to_str().unwrap(), "list --depth 2");
    assert_eq!(
        recursive.output.model_output,
        "alias/ -> nested/\nnested/\nnested/deeper/\n"
    );
}

#[test]
fn inspect_stat_keeps_rare_metadata_opt_in() {
    let root = temp_root("inspect-stat");
    fs::write(root.join("target.txt"), "content\n").unwrap();

    let default = execute_stat_command(root.to_str().unwrap(), "stat target.txt");
    assert!(default.succeeded);
    assert!(
        default
            .output
            .model_output
            .contains("target.txt\nsize: 8 bytes\n")
    );
    assert!(default.output.model_output.contains("modified: "));
    assert!(default.output.model_output.contains("permissions: "));
    assert!(!default.output.model_output.contains("inode:"));
    assert!(!default.output.model_output.contains("uid:"));

    let extended = execute_stat_command(root.to_str().unwrap(), "stat target.txt --metadata");
    assert!(extended.output.model_output.contains("inode:"));
    assert!(extended.output.model_output.contains("uid:"));
    assert!(extended.output.model_output.contains("blocks:"));
}

#[test]
fn inspect_stat_formats_unix_timestamps() {
    assert_eq!(
        format_unix_timestamp(0, 0, false),
        "1970-01-01 00:00:00 UTC"
    );
    assert_eq!(
        format_unix_timestamp(1_709_164_800, 123_456_789, true),
        "2024-02-29 00:00:00.123456789 UTC"
    );
}

#[test]
fn inspect_bytes_returns_contiguous_hex_and_exact_navigation_offsets() {
    let root = temp_root("inspect-bytes");
    fs::write(
        root.join("signature.bin"),
        [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
    )
    .unwrap();

    let execution = execute_bytes_command(root.to_str().unwrap(), "bytes signature.bin 0+4");
    assert!(execution.succeeded);
    assert_eq!(
        execution.output.model_output,
        "signature.bin 8 bytes\nrange: 0+4\n89504e47\nnext: 4+4\n"
    );
    assert!(!execution.output.model_output.contains("89 50"));
    let unsupported =
        execute_bytes_command(root.to_str().unwrap(), "bytes signature.bin 0+4 --exact");
    assert!(!unsupported.succeeded);
    assert_eq!(
        unsupported.output.model_output,
        "failed to parse `inspect` bytes input: unsupported option `--exact`"
    );
}

#[test]
fn inspect_bytes_rejects_unbounded_context_output() {
    let root = temp_root("inspect-bytes-limit");
    fs::write(root.join("large.bin"), [0_u8; 4]).unwrap();

    let execution = execute_bytes_command(root.to_str().unwrap(), "bytes large.bin 0+16385");
    assert!(!execution.succeeded);
    assert_eq!(
        execution.output.model_output,
        "failed to parse `inspect` bytes input: length must not exceed 16384 bytes"
    );
}

#[test]
fn inspect_byte_search_finds_overlapping_matches_across_read_chunks() {
    let root = temp_root("inspect-byte-search");
    let mut data = vec![0_u8; 16_383];
    data.extend_from_slice(b"aaaa");
    fs::write(root.join("target.bin"), data).unwrap();

    let execution =
        execute_byte_search_command(root.to_str().unwrap(), "byte-search target.bin 616161");
    assert!(execution.succeeded);
    assert_eq!(execution.output.model_output, "16383\n16384\n");
}

#[test]
fn inspect_strings_reports_utf8_byte_offsets_and_filters_runs() {
    let root = temp_root("inspect-strings");
    fs::write(root.join("target.bin"), b"\0hello\0h\xc3\xa9llo\0other\0").unwrap();

    let all = execute_strings_command(root.to_str().unwrap(), "strings target.bin");
    assert!(all.succeeded);
    assert_eq!(all.output.model_output, "1 hello\n7 héllo\n14 other\n");

    let filtered = execute_strings_command(root.to_str().unwrap(), "strings target.bin \"éll\"");
    assert!(filtered.succeeded);
    assert_eq!(filtered.output.model_output, "7 héllo\n");
}

#[test]
fn inspect_elf_maps_symbols_bytes_and_dynamic_linking_metadata() {
    let root = temp_root("inspect-elf");
    fs::write(
        root.join("probe.c"),
        "#include <stdio.h>\n__attribute__((noinline)) int introspection_probe(int value) { return value + 7; }\nint main(void) { puts(\"elf-introspection-probe\"); return introspection_probe(0); }\n",
    )
    .unwrap();
    let compile = Command::new("cc")
        .args(["-g", "-O0", "-o", "probe", "probe.c"])
        .current_dir(&root)
        .output()
        .expect("execute C compiler");
    assert!(
        compile.status.success(),
        "C compiler failed with status {}\nstdout:\n{}\nstderr:\n{}",
        compile.status,
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let compile_object = Command::new("cc")
        .args(["-g", "-O0", "-c", "-o", "probe.o", "probe.c"])
        .current_dir(&root)
        .output()
        .expect("execute C compiler for relocatable object");
    assert!(
        compile_object.status.success(),
        "C object compilation failed with status {}\nstdout:\n{}\nstderr:\n{}",
        compile_object.status,
        String::from_utf8_lossy(&compile_object.stdout),
        String::from_utf8_lossy(&compile_object.stderr)
    );
    let cwd = root.to_str().unwrap();

    let summary = execute_elf_command(cwd, "elf probe");
    assert!(summary.succeeded);
    assert!(summary.output.model_output.starts_with("ELF"));
    assert!(summary.output.model_output.contains("entry virtual "));
    assert!(summary.output.model_output.contains("entry file "));

    let sections = execute_elf_command(cwd, "elf probe sections");
    assert!(sections.succeeded);
    assert!(sections.output.model_output.contains(".text file "));

    let symbols = execute_elf_command(cwd, "elf probe symbols introspection_probe");
    assert!(symbols.succeeded);
    let symbol_line = symbols.output.model_output.lines().next().unwrap();
    assert!(symbol_line.starts_with("introspection_probe virtual "));
    let fields = symbol_line.split_whitespace().collect::<Vec<_>>();
    let virtual_address = fields[2].split_once('+').unwrap().0;
    let file_index = fields.iter().position(|field| *field == "file").unwrap();
    let file_offset = fields[file_index + 1];
    let object_symbols = execute_elf_command(cwd, "elf probe.o symbols introspection_probe");
    assert!(object_symbols.succeeded);
    assert!(object_symbols.output.model_output.contains("section .text"));

    let address = execute_elf_command(cwd, &format!("elf probe address {virtual_address}"));
    assert!(address.succeeded);
    assert!(
        address
            .output
            .model_output
            .contains(&format!("file {file_offset}\n"))
    );

    let offset = execute_elf_command(cwd, &format!("elf probe offset {file_offset}"));
    assert!(offset.succeeded);
    assert!(
        offset
            .output
            .model_output
            .contains(&format!("virtual {virtual_address}\n"))
    );

    let relocations = execute_elf_command(cwd, "elf probe relocations puts");
    assert!(relocations.succeeded);
    assert!(relocations.output.model_output.contains("target puts"));

    let dynamic = execute_elf_command(cwd, "elf probe dynamic");
    assert!(dynamic.succeeded);
    assert!(dynamic.output.model_output.contains("needed "));
    assert!(dynamic.output.model_output.contains("import "));
    assert!(dynamic.output.model_output.contains("puts"));
}

#[test]
fn inspect_path_command_search_ranks_matches_and_respects_path_precedence() {
    let root = temp_root("inspect-path-commands");
    let first = root.join("first");
    let second = root.join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();

    for path in [
        first.join("mycmd"),
        first.join("mycmd-helper"),
        first.join("my-command"),
        second.join("mycmd"),
    ] {
        fs::write(&path, "").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
    fs::write(first.join("mycmd-hidden"), "").unwrap();

    let path = std::env::join_paths([&first, &second]).unwrap();
    let matches = search_path_commands(&root, &path, "myc");
    let names = matches
        .iter()
        .map(|command_match| command_match.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["mycmd", "mycmd-helper"]);
    assert_eq!(matches[0].path, first.join("mycmd"));

    let exact_matches = search_path_commands(&root, &path, "mycmd");
    assert_eq!(exact_matches.len(), 1);
    assert_eq!(exact_matches[0].name, "mycmd");
    assert_eq!(exact_matches[0].path, first.join("mycmd"));
}

#[test]
fn shell_word_parser_reads_typical_fff_command() {
    assert_eq!(
        parse_shell_words(
            "fff \"UnsupportedUntypedLet|UntypedLet|fs.read_to_string|read_to_string|fn analyze_source_file|analyze_source_file\" src tests/effectcheck_subset.rs",
        )
        .unwrap(),
        vec![
            ShellWord { value: "fff".to_string(), quoted: false },
            ShellWord {
                value: "UnsupportedUntypedLet|UntypedLet|fs.read_to_string|read_to_string|fn analyze_source_file|analyze_source_file".to_string(),
                quoted: true,
            },
            ShellWord { value: "src".to_string(), quoted: false },
            ShellWord { value: "tests/effectcheck_subset.rs".to_string(), quoted: false },
        ]
    );
}

#[test]
fn fff_ignores_leading_dash_inside_quoted_pattern() {
    let root = temp_root("fff-quoted-leading-dash");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/lib.rs"), "fn - notanargument() {}\n").unwrap();

    let output =
        execute_search_command(project.to_str().unwrap(), "search \"- notanargument\"").output;
    assert!(
        output
            .model_output
            .contains("src/lib.rs\n1 fn - notanargument() {}")
    );
}

#[test]
fn fff_pipeline_supports_head_tail_and_recursive_fff_filters() {
    let root = temp_root("fff-pipeline");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("src/lib.rs"),
        "fn alpha() {}\nfn beta() {}\nfn beta_destroy() {}\nfn gamma_destroy() {}\n",
    )
    .unwrap();

    let head = execute_search_command(project.to_str().unwrap(), "search fn src | head 3").output;
    assert!(head.model_output.contains("src/lib.rs\n1 fn alpha() {}"));
    assert!(head.model_output.contains("2 fn beta() {}"));
    assert!(!head.model_output.contains("3 fn beta_destroy() {}"));

    let tail = execute_search_command(project.to_str().unwrap(), "search fn src | tail 2").output;
    assert!(tail.model_output.contains("3 fn beta_destroy() {}"));
    assert!(tail.model_output.contains("4 fn gamma_destroy() {}"));
    assert!(!tail.model_output.contains("1 fn alpha() {}"));

    let filtered = execute_search_command(
        project.to_str().unwrap(),
        "search fn src | search -F destroy",
    )
    .output;
    assert!(
        filtered
            .model_output
            .contains("src/lib.rs\n3 fn beta_destroy() {}")
    );
    assert!(filtered.model_output.contains("4 fn gamma_destroy() {}"));
    assert!(!filtered.model_output.contains("1 fn alpha() {}"));
}

#[test]
fn fff_files_lists_paths_and_supports_glob_constraints() {
    let root = temp_root("fff-files");
    let project = root.join("project");
    fs::create_dir_all(project.join("src/bin")).unwrap();
    fs::create_dir_all(project.join("target")).unwrap();
    fs::write(project.join("src/lib.rs"), "").unwrap();
    fs::write(project.join("src/bin/main.rs"), "").unwrap();
    fs::write(project.join("src/readme.md"), "").unwrap();
    fs::write(project.join("target/generated.rs"), "").unwrap();

    let output = execute_search_command(
        project.to_str().unwrap(),
        "search --files src --glob \"*.rs\"",
    )
    .output;
    assert!(output.model_output.contains("src/bin/main.rs\n"));
    assert!(!output.model_output.contains("src/readme.md"));
    assert!(!output.model_output.contains("target/generated.rs"));
}

#[test]
fn fff_search_exclude_filters_matching_paths() {
    let root = temp_root("fff-search-exclude");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/keep.rs"), "fn needle() {}\n").unwrap();
    fs::write(project.join("src/skip.generated.rs"), "fn needle() {}\n").unwrap();
    fs::write(project.join("src/skip_test.rs"), "fn needle() {}\n").unwrap();

    let output = execute_search_command(
        project.to_str().unwrap(),
        "search needle src --exclude \"*.generated.rs\" --exclude \"*_test.rs\"",
    )
    .output;
    assert!(
        output
            .model_output
            .contains("src/keep.rs\n1 fn needle() {}")
    );
    assert!(
        !output.model_output.contains("src/skip.generated.rs"),
        "excluded glob should not appear: {}",
        output.model_output
    );
    assert!(
        !output.model_output.contains("src/skip_test.rs"),
        "excluded glob should not appear: {}",
        output.model_output
    );
}

#[test]
fn fff_search_exclude_with_equals_form() {
    let root = temp_root("fff-search-exclude-equals");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/keep.rs"), "fn needle() {}\n").unwrap();
    fs::write(project.join("src/skip.rs"), "fn needle() {}\n").unwrap();

    let output = execute_search_command(
        project.to_str().unwrap(),
        "search needle src --exclude=src/skip.rs",
    )
    .output;
    assert!(
        output
            .model_output
            .contains("src/keep.rs\n1 fn needle() {}")
    );
    assert!(
        !output.model_output.contains("src/skip.rs\n"),
        "excluded path should not appear: {}",
        output.model_output
    );
}

#[test]
fn fff_files_exclude_filters_matching_paths() {
    let root = temp_root("fff-files-exclude");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/keep.rs"), "").unwrap();
    fs::write(project.join("src/skip.generated.rs"), "").unwrap();

    let output = execute_search_command(
        project.to_str().unwrap(),
        "search --files src --exclude \"*.generated.rs\"",
    )
    .output;
    assert!(output.model_output.contains("src/keep.rs\n"));
    assert!(
        !output.model_output.contains("src/skip.generated.rs"),
        "excluded glob should not appear: {}",
        output.model_output
    );
}
#[test]
fn fff_files_flag_is_order_independent() {
    let root = temp_root("fff-files-option-order");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/keep.rs"), "").unwrap();
    fs::write(project.join("src/skip.txt"), "").unwrap();

    let output = execute_search_command(
        project.to_str().unwrap(),
        "search src --glob \"*.rs\" --files",
    )
    .output;
    assert_eq!(output.model_output, "src/keep.rs\n");
}

#[test]
fn fff_files_flag_rejects_duplicates() {
    let words = parse_shell_words("search --files src --files").unwrap();
    let error = parse_fff_pipeline(".", &words).unwrap_err();
    assert_eq!(error, "option `--files` may only be specified once");
}

#[test]
fn fff_searches_and_lists_out_of_project_paths() {
    let root = temp_root("fff-outside-project");
    let project = root.join("project");
    let external = root.join("external");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join("outside.rs"), "fn outside_symbol() {}\n").unwrap();
    fs::write(external.join("outside.txt"), "outside text\n").unwrap();

    let search_command = format!("search outside_symbol {}", external.display());
    let search = execute_search_command(project.to_str().unwrap(), &search_command).output;
    assert!(
        search
            .model_output
            .contains("external/outside.rs\n1 fn outside_symbol() {}")
    );

    let files_command = format!("search --files {} --glob \"*.rs\"", external.display());
    let files = execute_search_command(project.to_str().unwrap(), &files_command).output;
    assert!(files.model_output.contains("external/outside.rs\n"));
    assert!(!files.model_output.contains("external/outside.txt"));
}

#[test]
fn fff_parser_preserves_parentheses_and_backslashes_in_quoted_patterns() {
    let words = parse_shell_words(r#"fff "lock\(\)|borrow\(|Cell<" src --glob "*.rs""#).unwrap();

    assert_eq!(words[1].value, r"lock\(\)|borrow\(|Cell<");
    assert_eq!(words[4].value, "*.rs");
    assert!(words[1].quoted);
    assert!(words[4].quoted);
}

#[test]
fn shell_word_parser_preserves_backslash_before_non_special_chars_in_double_quotes() {
    // LLMs often write search patterns like "while \[ ! -f /tmp/lock" where \[
    // is meant to be passed literally to search's regex engine. Inside double
    // quotes only $, `, ", \, and newline are POSIX-special after a backslash;
    // everything else keeps both the backslash and the following character.
    let words = parse_shell_words(r#"search "package-lock|sleep 300|while \[ ! -f /tmp/lock" src"#)
        .unwrap();
    assert_eq!(words.len(), 3);
    assert_eq!(
        words[1].value,
        r"package-lock|sleep 300|while \[ ! -f /tmp/lock"
    );
    assert!(words[1].quoted);
}

#[test]
fn shell_word_parser_still_strips_posix_special_escapes_in_double_quotes() {
    // \" ends a quote (handled separately) but \\ and \$ should have the
    // backslash consumed, yielding just the following character.
    let words = parse_shell_words(r#""foo\\bar""#).unwrap();
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].value, r"foo\bar");
}

#[test]
fn process_status_filter_reports_no_results() {
    let execution = InspectCommandExecution {
        output: NativeToolExecutionOutput::same(
            "USER PID COMMAND\nroot 1 /sbin/init\nme 42 cargo test\n".to_string(),
        ),
        succeeded: true,
    };

    let no_matches = filter_process_status_output(execution.clone(), "missing");
    assert_eq!(no_matches.output.model_output, "no results\n");
    assert_eq!(no_matches.output.display_output, "no results\n");
    assert!(no_matches.succeeded);

    let matches = filter_process_status_output(execution, "CARGO");
    assert_eq!(
        matches.output.model_output,
        "USER PID COMMAND\nme 42 cargo test\n"
    );
    assert!(matches.succeeded);
}

#[test]
fn inspection_display_outputs_use_ansi_styling() {
    assert_eq!(
        colorize_fff_display_output("src/lib.rs\n42 fn main() {}\n6012 single file match\n"),
        format!(
            "{ANSI_CYAN}src/lib.rs{ANSI_RESET}\n{ANSI_GREEN}42{ANSI_RESET} fn main() {{}}\n{ANSI_GREEN}6012{ANSI_RESET} single file match\n"
        )
    );
    assert_eq!(
        colorize_process_status_display_output("  1234 /bin/bash\n"),
        format!("  {ANSI_GREEN}1234{ANSI_RESET} /bin/bash\n")
    );
}
#[test]
fn mixed_inspect_display_is_compact_and_colored() {
    let results = vec![
        InspectJobResult {
            job: InspectJob::Read(InspectReadJob {
                path: "Cargo.toml".to_string(),
                ranges: vec![InspectReadRange {
                    start_line: 1,
                    line_count: 1,
                }],
            }),
            output: NativeToolExecutionOutput::split("142[workspace]\n".to_string(), String::new()),
        },
        InspectJobResult {
            job: InspectJob::Command(InspectCommandJob {
                command: "search needle src".to_string(),
            }),
            output: NativeToolExecutionOutput::split(
                "src/lib.rs\n7 needle\n".to_string(),
                colorize_fff_display_output("src/lib.rs\n7 needle\n"),
            ),
        },
        InspectJobResult {
            job: InspectJob::Command(InspectCommandJob {
                command: "ps systemd".to_string(),
            }),
            output: NativeToolExecutionOutput::split(
                "  1 systemd\n".to_string(),
                colorize_process_status_display_output("  1 systemd\n"),
            ),
        },
    ];

    assert_eq!(
        format_inspect_display_output(&results),
        format!(
            "{ANSI_CYAN}src/lib.rs{ANSI_RESET}\n{ANSI_GREEN}7{ANSI_RESET} needle\n  {ANSI_GREEN}1{ANSI_RESET} systemd\n"
        )
    );
}

#[tokio::test]
async fn inspect_tool_executes_read_jobs_in_request_order() {
    let root = temp_root("inspect-read-order");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("alpha.txt"), "a1\na2\na3\n").unwrap();
    fs::write(project.join("beta.txt"), "b1\nb2\nb3\n").unwrap();

    let output = execute_inspect_tool(
        project.to_str().unwrap().to_string(),
        "read alpha.txt\n2+1\nread beta.txt\n1-2\n",
    )
    .await;

    let alpha_anchor = format_line_anchor(2, edit_line_hash("a2"));
    let beta_anchor_1 = format_line_anchor(1, edit_line_hash("b1"));
    let beta_anchor_2 = format_line_anchor(2, edit_line_hash("b2"));
    assert_eq!(
        output.model_output,
        format!("{alpha_anchor}a2\nnext: 3+1\n{beta_anchor_1}b1\n{beta_anchor_2}b2\nnext: 3+2\n")
    );
    assert_eq!(output.display_output, "");
}

#[tokio::test]
async fn inspect_search_parse_error_does_not_reject_other_jobs() {
    let root = temp_root("inspect-search-error-isolation");
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/lib.rs"), "fn needle() {}\n").unwrap();

    let output = execute_inspect_tool(
        project.to_str().unwrap().to_string(),
        "search needle src\nsearch --definitely-unsupported needle\n",
    )
    .await;
    assert!(output.model_output.contains("src/lib.rs\n1 fn needle() {}"));
    assert!(output.model_output.contains(
        "failed to parse `inspect` search input: unsupported option `--definitely-unsupported`"
    ));
}

#[tokio::test]
async fn inspect_transcript_renders_read_output_as_structured_display() {
    let root = temp_root("inspect-transcript-display");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("alpha.txt"), "a1\na2\na3\n").unwrap();
    fs::write(project.join("beta.txt"), "b1\nb2\nb3\n").unwrap();
    let mut config = test_config(root.clone());
    config.cwd = project.to_str().unwrap().to_string();
    config.tool_registry = terminal_tool_registry();
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "read two files".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial inspect submit").await;
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: crate::tools::FreeformToolCall {
                call_id: "call-inspect".to_string(),
                name: INSPECT_TOOL_NAME.to_string(),
                input: "read alpha.txt\n2+1\nread beta.txt\n1+2\n".to_string(),
            },
        })
        .await
        .unwrap();

    let RuntimeEvent::FreeformToolOutput(output) = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::FreeformToolOutput(output) if output.call_id == "call-inspect")
    })
    .await
    else {
        unreachable!("matched freeform tool output")
    };

    assert!(output.output.contains("2daa2\nnext: 3+1\n"));
    assert_eq!(output.display_output, Some(String::new()));
    let Some(ToolOutputDisplayRecord::InspectRead(reads)) = output.display else {
        panic!("expected structured inspect read display");
    };
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0].path, "alpha.txt");
    assert_eq!(reads[0].start_line, 2);
    assert_eq!(reads[0].lines, vec!["a2".to_string()]);
    assert_eq!(
        reads[0].next,
        Some(InspectReadNextRecord {
            start_line: 3,
            line_count: 1
        })
    );
    assert_eq!(reads[1].path, "beta.txt");
    assert_eq!(reads[1].start_line, 1);
    assert_eq!(reads[1].lines, vec!["b1".to_string(), "b2".to_string()]);
}

#[test]
fn staged_patch_tool_input_parser_rejects_extra_lines() {
    assert_eq!(
        parse_staged_patch_tool_input("patch: sp-1\n").unwrap(),
        "sp-1"
    );

    let error = parse_staged_patch_tool_input("patch: sp-1\nextra: nope").unwrap_err();
    assert_eq!(
        error,
        "failed to parse staged patch input: only one `patch:` line is allowed"
    );
}

#[tokio::test]
async fn persist_slash_command_continues_after_response_completion() {
    let root = temp_root("persist-continuation");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let mut config = test_config(root);
    config.tool_registry = NativeToolRegistry::codex();
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "finish the implementation".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial submit").await;
    assert!(response_tool_names(&request).contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string()));

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/persist".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: enabled"),
    )
    .await;
    assert!(responses_rx.try_recv().is_err());

    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::Frame {
            request_id: request.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "first pass"
            }),
        })
        .await
        .unwrap();
    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();

    let (continuation, continuation_events) =
        next_responses_submit(&responses_rx, "persist continuation").await;
    assert!(response_tool_names(&continuation).contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string()));
    let continuation_input = continuation
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let persist_message = message_input_text(continuation_input.last().unwrap());
    assert!(persist_message.contains("Persisted task:\nfinish the implementation"));
    assert!(persist_message.contains("mark_task_complete"));

    continuation_events
        .send(ResponsesEvent::RequestStarted(continuation.request_id))
        .await
        .unwrap();
    continuation_events
        .send(ResponsesEvent::Frame {
            request_id: continuation.request_id,
            body: json!({
                "type": "response.output_text.delta",
                "delta": "second pass"
            }),
        })
        .await
        .unwrap();
    continuation_events
        .send(ResponsesEvent::RequestCompleted(continuation.request_id))
        .await
        .unwrap();

    let (second_continuation, _) =
        next_responses_submit(&responses_rx, "second persist continuation").await;
    assert!(
        response_tool_names(&second_continuation)
            .contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string())
    );
}

#[tokio::test]
async fn persist_slash_command_toggles_off_before_response_completion() {
    let root = temp_root("persist-toggle-off");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let mut config = test_config(root);
    config.tool_registry = NativeToolRegistry::codex();
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "finish the implementation".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial submit").await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/persist".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: enabled"),
    )
    .await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/persist".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: disabled"),
    )
    .await;

    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), responses_rx.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn persist_interrupt_pauses_continuation_and_resumes() {
    let root = temp_root("persist-interrupt-pause");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let mut config = test_config(root);
    config.tool_registry = NativeToolRegistry::codex();
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "finish the implementation".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial submit").await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/persist".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: enabled"),
    )
    .await;

    // Send empty steering interrupt immediately.
    runtime
        .commands
        .send(RuntimeCommand::ApplySteering {
            text: "".to_string(),
            mode: SteeringMode::InterruptNow,
        })
        .await
        .unwrap();

    // Verify it is paused.
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: paused"),
    )
    .await;

    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();

    // Verify it does NOT submit a new request (stays paused).
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), responses_rx.recv())
            .await
            .is_err()
    );

    // Continue using /persist continue
    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/persist continue".to_string(),
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: continued"),
    )
    .await;

    // Verify it started a new responses request for the continuation.
    let (continuation, continuation_events) =
        next_responses_submit(&responses_rx, "persist continuation").await;

    // Now test interrupt and resume with another message.
    runtime
        .commands
        .send(RuntimeCommand::ApplySteering {
            text: "".to_string(),
            mode: SteeringMode::InterruptNow,
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: paused"),
    )
    .await;

    continuation_events
        .send(ResponsesEvent::RequestCompleted(continuation.request_id))
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), responses_rx.recv())
            .await
            .is_err()
    );

    // Submit another user message.
    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "new user instruction".to_string(),
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: continued"),
    )
    .await;

    // Verify it started a responses request for the new message.
    let (new_request, new_events) =
        next_responses_submit(&responses_rx, "new request submit").await;

    // After this finishes, it should auto-continue since persist is still active and unpaused.
    new_events
        .send(ResponsesEvent::RequestCompleted(new_request.request_id))
        .await
        .unwrap();

    let (auto_continuation, _) = next_responses_submit(&responses_rx, "auto continuation").await;
    assert!(
        response_tool_names(&auto_continuation).contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string())
    );
}

#[tokio::test]
async fn persist_pause_command_pauses_without_interruption() {
    let root = temp_root("persist-pause-cmd");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let mut config = test_config(root);
    config.tool_registry = NativeToolRegistry::codex();
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "finish the implementation".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial submit").await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/persist".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: enabled"),
    )
    .await;

    // Pause persist mode.
    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/persist pause".to_string(),
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: paused"),
    )
    .await;

    // Verify that the request is NOT interrupted (no abort event or completed event yet, connection still active).
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), responses_rx.recv())
            .await
            .is_err()
    );

    // Complete the request.
    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();

    // Verify it does NOT submit a new request (stays paused).
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), responses_rx.recv())
            .await
            .is_err()
    );

    // Send another user message to resume.
    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "please keep going".to_string(),
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: continued"),
    )
    .await;

    // Verify a new responses request is submitted for the message.
    let (new_request, _) = next_responses_submit(&responses_rx, "resumed submit").await;
    assert!(response_tool_names(&new_request).contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string()));
}

#[tokio::test]
async fn persist_completion_tool_call_stops_without_tool_output() {
    let root = temp_root("persist-complete-accepted");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let mut config = test_config(root.clone());
    config.tool_registry = NativeToolRegistry::codex();
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "finish the implementation".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial submit").await;
    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/persist".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: enabled"),
    )
    .await;

    events
        .send(ResponsesEvent::RequestCompleted(request.request_id))
        .await
        .unwrap();
    let (continuation, continuation_events) =
        next_responses_submit(&responses_rx, "persist continuation").await;
    assert!(response_tool_names(&continuation).contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string()));

    continuation_events
        .send(ResponsesEvent::RequestStarted(continuation.request_id))
        .await
        .unwrap();
    continuation_events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: continuation.request_id,
            call: crate::tools::FreeformToolCall {
                call_id: "call-complete".to_string(),
                name: MARK_TASK_COMPLETE_TOOL_NAME.to_string(),
                input: "complete".to_string(),
            },
        })
        .await
        .unwrap();

    let _ = next_runtime_event_matching(
        &runtime,
        |event| matches!(event, RuntimeEvent::TranscriptLine(line) if line == "persist: complete"),
    )
    .await;

    continuation_events
        .send(ResponsesEvent::RequestCompleted(continuation.request_id))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), responses_rx.recv())
            .await
            .is_err()
    );

    let summary = {
        let mut index = crate::sessions::SessionIndex::load(&root).unwrap();
        index
            .summary_by_id(&SessionId::new("harness-session"))
            .unwrap()
            .unwrap()
    };
    let records = SessionStore::new(&root).read_records(summary.path).unwrap();
    assert!(!records.iter().any(|record| {
        matches!(
            &record.kind,
            SessionRecordKind::FreeformToolCall(call)
                if call.call_id == "call-complete" && call.name == MARK_TASK_COMPLETE_TOOL_NAME
        )
    }));
    assert!(!records.iter().any(|record| {
        matches!(
            &record.kind,
            SessionRecordKind::FreeformToolOutput(output)
                if output.call_id == "call-complete"
        )
    }));
}

#[tokio::test]
async fn persist_completion_tool_call_respects_user_request_not_to_end() {
    let root = temp_root("persist-complete-denied");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let mut config = test_config(root.clone());
    config.tool_registry = NativeToolRegistry::codex();
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/persist dont mark the goal complete, keep working".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "persist explicit task").await;
    assert!(response_tool_names(&request).contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string()));
    let input = request
        .body
        .as_object()
        .unwrap()
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    let persist_message = message_input_text(&input[1]);
    assert!(persist_message.contains("Persisted task:\ndont mark the goal complete, keep working"));

    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: crate::tools::FreeformToolCall {
                call_id: "call-complete".to_string(),
                name: MARK_TASK_COMPLETE_TOOL_NAME.to_string(),
                input: "complete".to_string(),
            },
        })
        .await
        .unwrap();

    let output = next_runtime_event_matching(&runtime, |event| {
        matches!(
            event,
            RuntimeEvent::FreeformToolOutput(output)
                if output.call_id == "call-complete"
                    && output.output.contains("user requested persist not be ended")
        )
    })
    .await;
    let RuntimeEvent::FreeformToolOutput(output) = output else {
        unreachable!("matched typed tool output")
    };
    assert!(
        output
            .output
            .contains("user requested persist not be ended")
    );

    let (next_continuation, _) =
        next_responses_submit(&responses_rx, "persist continues after denied completion").await;
    assert!(
        response_tool_names(&next_continuation).contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string())
    );

    let summary = {
        let mut index = crate::sessions::SessionIndex::load(&root).unwrap();
        index
            .summary_by_id(&SessionId::new("harness-session"))
            .unwrap()
            .unwrap()
    };
    let records = SessionStore::new(&root).read_records(summary.path).unwrap();
    assert!(records.iter().any(|record| {
        matches!(
            &record.kind,
            SessionRecordKind::FreeformToolCall(call)
                if call.call_id == "call-complete" && call.name == MARK_TASK_COMPLETE_TOOL_NAME
        )
    }));
    assert!(records.iter().any(|record| {
        matches!(
            &record.kind,
            SessionRecordKind::FreeformToolOutput(output)
                if output.call_id == "call-complete"
        )
    }));
}

#[tokio::test]
async fn mark_task_complete_without_persist_returns_persist_not_active() {
    let root = temp_root("persist-complete-inactive");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let mut config = test_config(root);
    config.tool_registry = NativeToolRegistry::codex();
    let runtime = HarnessActor::spawn(config, responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "finish normally".to_string(),
        })
        .await
        .unwrap();

    let (request, events) = next_responses_submit(&responses_rx, "initial submit").await;
    assert!(response_tool_names(&request).contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string()));

    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: crate::tools::FreeformToolCall {
                call_id: "call-complete".to_string(),
                name: MARK_TASK_COMPLETE_TOOL_NAME.to_string(),
                input: "complete".to_string(),
            },
        })
        .await
        .unwrap();

    let output = next_runtime_event_matching(&runtime, |event| {
        matches!(
            event,
            RuntimeEvent::FreeformToolOutput(output)
                if output.call_id == "call-complete"
                    && output.output.contains("persist not active")
        )
    })
    .await;
    let RuntimeEvent::FreeformToolOutput(output) = output else {
        unreachable!("matched typed tool output")
    };
    assert!(output.output.contains("persist not active"));

    let (continuation, _) =
        next_responses_submit(&responses_rx, "continuation after inactive completion").await;
    assert!(response_tool_names(&continuation).contains(&MARK_TASK_COMPLETE_TOOL_NAME.to_string()));
}

#[tokio::test]
async fn unknown_freeform_tool_call_persists_output_and_submits_continuation() {
    let root = temp_root("unknown-freeform-tool-continuation");
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(test_config(root.clone()), responses);

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "call custom tool".to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, events } = responses_rx.recv().await.unwrap() else {
        panic!("expected initial submit command");
    };
    events
        .send(ResponsesEvent::RequestStarted(request.request_id))
        .await
        .unwrap();
    events
        .send(ResponsesEvent::FreeformToolCall {
            request_id: request.request_id,
            call: crate::tools::FreeformToolCall {
                call_id: "call-freeform".to_string(),
                name: "other_tool".to_string(),
                input: "raw input".to_string(),
            },
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit {
        request: continuation,
        ..
    } = tokio::time::timeout(std::time::Duration::from_secs(1), responses_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected continuation submit command");
    };
    let body = continuation.body.as_object().unwrap();
    let input = body
        .get(&"input")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(input.len(), 3);

    let call_item = input[1].as_object().unwrap();
    assert_eq!(
        call_item.get(&"type").and_then(JsonValueTrait::as_str),
        Some("custom_tool_call")
    );
    assert_eq!(
        call_item.get(&"name").and_then(JsonValueTrait::as_str),
        Some("other_tool")
    );

    let output_item = input[2].as_object().unwrap();
    assert_eq!(
        output_item.get(&"type").and_then(JsonValueTrait::as_str),
        Some("custom_tool_call_output")
    );
    assert_eq!(
        output_item.get(&"call_id").and_then(JsonValueTrait::as_str),
        Some("call-freeform")
    );
    assert_eq!(
        output_item.get(&"output").and_then(JsonValueTrait::as_str),
        Some("freeform tool execution is not implemented: other_tool")
    );

    let summary = {
        let mut index = crate::sessions::SessionIndex::load(&root).unwrap();
        index
            .summary_by_id(&SessionId::new("harness-session"))
            .unwrap()
            .unwrap()
    };
    let records = SessionStore::new(&root).read_records(summary.path).unwrap();
    assert!(matches!(
        records[3].kind,
        SessionRecordKind::FreeformToolCall(_)
    ));
    assert!(matches!(
        records[4].kind,
        SessionRecordKind::FreeformToolOutput(_)
    ));
}

#[tokio::test]
async fn model_slash_command_updates_context_window_policy() {
    let root = temp_root("model-command-context-policy");
    let mut config = test_config(root);
    config.model_catalog = vec![
        ResponsesModelInfo {
            slug: "gpt-test".to_string(),
            supports_parallel_tool_calls: true,
            supports_reasoning: true,
            context_window: Some(272_000),
            max_context_window: Some(1_000_000),
            auto_compact_token_limit: None,
            effective_context_window_percent: 95,
        },
        ResponsesModelInfo {
            slug: "gpt-5.5".to_string(),
            supports_parallel_tool_calls: true,
            supports_reasoning: true,
            context_window: Some(272_000),
            max_context_window: Some(1_000_000),
            auto_compact_token_limit: None,
            effective_context_window_percent: 95,
        },
    ];
    let (responses, responses_rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
    let runtime = HarnessActor::spawn(config, responses);

    skip_provider_changed_event(&runtime).await;

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "/model gpt-test xhigh fast".to_string(),
        })
        .await
        .unwrap();
    let _ = next_runtime_event_matching(&runtime, |event| {
        matches!(event, RuntimeEvent::ModelSettingsChanged(_))
    })
    .await;
    assert!(responses_rx.try_recv().is_err());

    runtime
        .commands
        .send(RuntimeCommand::SubmitInput {
            text: "this input should cross the small switched model compaction threshold"
                .to_string(),
        })
        .await
        .unwrap();

    let ResponsesCommand::Submit { request, .. } = responses_rx.recv().await.unwrap() else {
        panic!("expected compact submit command");
    };
    drop(request);
}

#[test]
fn history_records_wrap_freeform_tool_history_for_ollama_cloud() {
    let input = history_to_responses_input(
        &[
            HistoryRecord::FreeformToolCall(FreeformToolCallRecord {
                call_id: "call-1".to_string(),
                name: "terminal_open".to_string(),
                input: "input: pwd".to_string(),
            }),
            HistoryRecord::FreeformToolOutput(FreeformToolOutputRecord {
                call_id: "call-1".to_string(),
                output: "ok".to_string(),
                display_output: None,
                display: None,
            }),
        ],
        &crate::tools::NativeToolRegistry::ollama_cloud(),
    )
    .unwrap();

    let call = input[0].as_object().unwrap();
    assert_eq!(
        call.get(&"type").and_then(JsonValueTrait::as_str),
        Some("function_call")
    );
    assert_eq!(
        call.get(&"name").and_then(JsonValueTrait::as_str),
        Some("terminal_open")
    );
    let arguments = call
        .get(&"arguments")
        .and_then(JsonValueTrait::as_str)
        .unwrap();
    let arguments: Value = sonic_rs::from_str(arguments).unwrap();
    assert_eq!(
        arguments
            .as_object()
            .unwrap()
            .get(&"input")
            .and_then(JsonValueTrait::as_str),
        Some("input: pwd")
    );
    let output = input[1].as_object().unwrap();
    assert_eq!(
        output.get(&"type").and_then(JsonValueTrait::as_str),
        Some("function_call_output")
    );
    assert_eq!(
        output.get(&"output").and_then(JsonValueTrait::as_str),
        Some("ok")
    );
}

#[test]
fn assistant_text_delta_parser_reads_responses_delta() {
    assert_eq!(
        assistant_text_delta_from_frame(&json!({
            "type": "response.output_text.delta",
            "delta": "chunk"
        })),
        Some("chunk")
    );
}

#[test]
fn utc_date_conversion_is_stable() {
    assert_eq!(utc_date_from_unix_ms(0), "1970-01-01");
    assert_eq!(utc_date_from_unix_ms(1_717_171_717_000), "2024-05-31");
}

#[test]
fn session_records_rebuild_history_after_checkpoint() {
    let history = history_from_session_records(
        &[HistoryRecord::UserMessage(MessageRecord {
            text: "checkpoint".to_string(),
        })],
        &[SessionRecord {
            seq: 9,
            kind: SessionRecordKind::AssistantMessage(MessageRecord {
                text: "after".to_string(),
            }),
        }],
    );
    assert_eq!(history.len(), 2);
}

#[test]
fn default_headers_are_available_for_harness_config() {
    let headers = lean_codex_default_headers();
    assert!(!headers.is_empty());
}
