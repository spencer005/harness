use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use bytes::Bytes;
pub use harness_responses_api::{
    ApiEndpoint, ApiProvider, Auth, AuthError, CHATGPT_ACCOUNT_ID, CODEX_CLI_CLIENT_VERSION,
    CODEX_CLI_USER_AGENT, ChatGptAuthSession, ChatGptAuthTokens, CodexHeaders as ResponsesHeaders,
    DEFAULT_CODEX_ORIGINATOR, ManagedChatGptAuth, OPENAI_BETA_HEADER,
    OPENAI_BETA_RESPONSES_WEBSOCKETS_V2, OllamaModelInfoClient, ResponsesApiError,
    ResponsesModelInfo, ResponsesModelsClient, ResponsesModelsList, ResponsesStreamEvent,
    ResponsesStreamRequest, ResponsesTurnState, ResponsesWsPool, WsPoolConfig,
    lean_codex_default_headers,
};
use http::{
    HeaderMap, Request,
    header::{CONTENT_TYPE, USER_AGENT},
};
use http_body_util::{BodyExt, Full};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client as HyperClient, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    actors::{
        ActorHandle, ActorReceiver, ActorSender, DEFAULT_ACTOR_MAILBOX_CAPACITY,
        channel as actor_channel,
    },
    compact::{CompactPlanError, ContextWindowPolicy},
    tools::{
        FreeformToolCall, FreeformToolInputDelta, FunctionToolCall, FunctionToolInputDelta,
        NativeTool, create_tools_json_for_responses_api, parse_freeform_tool_call_from_frame,
        parse_freeform_tool_input_delta_from_frame, parse_function_tool_call_from_frame,
        parse_function_tool_input_delta_from_frame,
    },
};

/// Default model used by new harness sessions.
pub const DEFAULT_MODEL: &str = GPT_5_5_MODEL;
const GPT_5_5_MODEL: &str = "gpt-5.5";
const GPT_5_5_CONTEXT_WINDOW: u64 = 400_000;
const GPT_5_5_EFFECTIVE_CONTEXT_WINDOW: u64 = 300_000;
const GPT_5_5_TARGET_TOKENS_AFTER_COMPACTION: u64 = GPT_5_5_CONTEXT_WINDOW / 2;
/// Default reasoning effort used by Codex Fast mode.
pub const DEFAULT_REASONING_EFFORT: &str = "xhigh";

/// Responses service-tier value used for Codex Fast mode.
pub const FAST_SERVICE_TIER: &str = "priority";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
/// Monotonic identifier assigned to harness Responses requests.
pub struct ResponsesRequestId(pub u64);

static NEXT_RESPONSES_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

impl ResponsesRequestId {
    /// Allocate a new Responses request id.
    pub fn new() -> Self {
        let id = NEXT_RESPONSES_REQUEST_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("responses request id counter exhausted");
        Self(id)
    }
}

impl Default for ResponsesRequestId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Error)]
/// Error returned while building a Responses API request body.
pub enum ResponsesRequestBuildError {
    /// Failed to encode the typed request into a `sonic_rs::Value`.
    #[error("failed to serialize response.create request: {0}")]
    Serialize(#[from] sonic_rs::Error),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
/// Error returned while parsing model settings.
pub enum ResponsesModelSettingParseError {
    /// Service tier is not supported by the CLI model setting parser.
    #[error("unsupported service tier `{value}`; use fast, priority, flex, or none")]
    UnsupportedServiceTier {
        /// Caller-provided service tier.
        value: String,
    },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
/// Error returned while deriving context-window policy from model metadata.
pub enum ResponsesContextWindowPolicyError {
    /// The selected model lacks context-window metadata.
    #[error("model `{model}` did not include a context window")]
    MissingContextWindow {
        /// Model slug that lacks context-window metadata.
        model: String,
    },
    /// Context-window metadata was present but invalid.
    #[error(transparent)]
    InvalidPolicy(#[from] CompactPlanError),
}

/// Build a context-window compaction policy from Responses model metadata.
pub fn context_window_policy_from_model_info(
    model_info: &ResponsesModelInfo,
) -> Result<ContextWindowPolicy, ResponsesContextWindowPolicyError> {
    let policy_context =
        context_window_policy_context_from_model_info(model_info).ok_or_else(|| {
            ResponsesContextWindowPolicyError::MissingContextWindow {
                model: model_info.slug.clone(),
            }
        })?;
    let max_input_tokens = policy_context.effective_context_window;
    let compact_at_tokens = auto_compact_token_limit_from_model_info(model_info, policy_context);
    let target_tokens_after_compaction =
        target_tokens_after_compaction_from_policy_context(policy_context, compact_at_tokens);
    Ok(ContextWindowPolicy::new(
        max_input_tokens,
        compact_at_tokens,
        target_tokens_after_compaction,
    )?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextWindowPolicyContext {
    effective_context_window: u64,
    target_tokens_after_compaction: Option<u64>,
    apply_metadata_auto_compact_limit: bool,
}

fn context_window_policy_context_from_model_info(
    model_info: &ResponsesModelInfo,
) -> Option<ContextWindowPolicyContext> {
    if model_info.slug == GPT_5_5_MODEL {
        return Some(ContextWindowPolicyContext {
            effective_context_window: GPT_5_5_EFFECTIVE_CONTEXT_WINDOW,
            target_tokens_after_compaction: Some(GPT_5_5_TARGET_TOKENS_AFTER_COMPACTION),
            apply_metadata_auto_compact_limit: false,
        });
    }
    model_info
        .effective_context_window()
        .map(|effective_context_window| ContextWindowPolicyContext {
            effective_context_window,
            target_tokens_after_compaction: None,
            apply_metadata_auto_compact_limit: true,
        })
}

fn auto_compact_token_limit_from_model_info(
    model_info: &ResponsesModelInfo,
    policy_context: ContextWindowPolicyContext,
) -> u64 {
    let context_limit = (policy_context.effective_context_window * 95) / 100;
    if policy_context.apply_metadata_auto_compact_limit {
        return model_info
            .auto_compact_token_limit
            .map_or(context_limit, |config_limit| {
                config_limit.min(context_limit)
            });
    }
    context_limit
}

fn target_tokens_after_compaction_from_policy_context(
    policy_context: ContextWindowPolicyContext,
    compact_at_tokens: u64,
) -> u64 {
    policy_context
        .target_tokens_after_compaction
        .unwrap_or(compact_at_tokens / 2)
}

/// Model capability data returned by the Responses model metadata query.
///
/// The request builder keeps backend parallel tool calls disabled, but the
/// metadata is still reported for diagnostics and experiments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponsesModelCapabilities {
    /// Whether the selected model supports parallel tool calls.
    pub supports_parallel_tool_calls: bool,
    /// Whether the selected model supports reasoning parameters.
    pub supports_reasoning: bool,
}

impl ResponsesModelCapabilities {
    /// Construct model capability metadata from queried provider flags.
    pub fn new(supports_parallel_tool_calls: bool, supports_reasoning: bool) -> Self {
        Self {
            supports_parallel_tool_calls,
            supports_reasoning,
        }
    }

    /// Extract request capabilities from queried `/models` metadata.
    pub fn from_model_info(model_info: &ResponsesModelInfo) -> Self {
        Self {
            supports_parallel_tool_calls: model_supports_parallel_tool_calls(model_info),
            supports_reasoning: model_info.supports_reasoning,
        }
    }
}
fn model_supports_parallel_tool_calls(model_info: &ResponsesModelInfo) -> bool {
    model_info.slug == GPT_5_5_MODEL || model_info.supports_parallel_tool_calls
}

/// Typed builder for the WebSocket `response.create` request body.
///
/// This builder exposes native tool specs for the Responses API.
#[derive(Debug, Clone)]
pub struct ResponsesCreateRequest {
    /// Model id passed to the Responses API.
    pub model: String,
    /// Optional model instructions for the request.
    pub instructions: String,
    /// Responses API input items.
    pub input: Vec<sonic_rs::Value>,
    /// Native tool specs sent in `tools`.
    pub tools: Vec<NativeTool>,
    /// Tool choice policy. Defaults to `auto`.
    pub tool_choice: String,
    /// Whether the backend may issue parallel tool calls.
    ///
    /// The harness keeps this disabled and exposes explicit custom tools for
    /// any parallel execution it can execute safely.
    pub parallel_tool_calls: bool,
    /// Whether the backend should store the response.
    ///
    /// ChatGPT Codex Responses WebSocket requests require `false`.
    pub store: bool,
    /// Whether the backend should stream response events.
    pub stream: bool,
    /// Additional Responses API include paths.
    pub include: Vec<String>,
    /// Reasoning effort passed as `reasoning.effort`.
    pub reasoning_effort: Option<String>,
    /// Optional Responses service tier. Codex Fast mode is `priority`.
    pub service_tier: Option<String>,
}

impl ResponsesCreateRequest {
    /// Create a `response.create` builder.
    ///
    /// Backend-level parallel tool calls stay disabled; the harness exposes
    /// explicit custom tools for parallel execution it owns.
    pub fn new(model: impl Into<String>, _capabilities: ResponsesModelCapabilities) -> Self {
        Self {
            model: model.into(),
            instructions: String::new(),
            input: Vec::new(),
            tools: Vec::new(),
            tool_choice: "auto".to_string(),
            parallel_tool_calls: false,
            store: false,
            stream: true,
            include: Vec::new(),
            reasoning_effort: None,
            service_tier: None,
        }
    }

    /// Set request instructions.
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// Set raw Responses API input items.
    pub fn with_input(mut self, input: Vec<sonic_rs::Value>) -> Self {
        self.input = input;
        self
    }

    /// Set native tools for this request.
    pub fn with_tools(mut self, tools: Vec<NativeTool>) -> Self {
        self.tools = tools;
        self
    }

    /// Set model execution settings for this request.
    pub fn with_model_settings(mut self, settings: &ModelSettings) -> Self {
        self.model = settings.model.clone();
        self.reasoning_effort = settings.reasoning_effort.clone();
        self.service_tier = settings.service_tier.clone();
        self
    }

    /// Serialize this typed request into the JSON body sent over the WS pool.
    pub fn to_body(&self) -> Result<sonic_rs::Value, ResponsesRequestBuildError> {
        let tools = create_tools_json_for_responses_api(&self.tools)?;
        let reasoning = self
            .reasoning_effort
            .as_deref()
            .map(|effort| ResponsesReasoning { effort });
        Ok(sonic_rs::to_value(&ResponsesCreateRequestBody {
            request_type: "response.create",
            model: &self.model,
            instructions: &self.instructions,
            input: &self.input,
            tools,
            tool_choice: &self.tool_choice,
            parallel_tool_calls: self.parallel_tool_calls,
            store: self.store,
            stream: self.stream,
            include: &self.include,
            reasoning,
            service_tier: self.service_tier.as_deref(),
        })?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// User-selected model execution settings.
pub struct ModelSettings {
    /// Model slug sent to the Responses API.
    pub model: String,
    /// Optional reasoning effort sent as `reasoning.effort`.
    pub reasoning_effort: Option<String>,
    /// Optional Responses API service tier.
    pub service_tier: Option<String>,
}

impl ModelSettings {
    /// Create model settings from a model slug and optional execution options.
    pub fn new(
        model: impl Into<String>,
        reasoning_effort: Option<String>,
        service_tier: Option<String>,
    ) -> Self {
        Self {
            model: normalize_model_slug(&model.into()),
            reasoning_effort,
            service_tier,
        }
    }

    /// Return the default Codex Fast model settings.
    pub fn default_codex_fast() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            reasoning_effort: Some(DEFAULT_REASONING_EFFORT.to_string()),
            service_tier: Some(FAST_SERVICE_TIER.to_string()),
        }
    }

    /// Return a user-facing service tier label.
    pub fn display_service_tier(&self) -> &str {
        match self.service_tier.as_deref() {
            Some(FAST_SERVICE_TIER) => "fast",
            Some(value) => value,
            None => "default",
        }
    }

    /// Return a user-facing reasoning effort label.
    pub fn display_reasoning_effort(&self) -> &str {
        self.reasoning_effort.as_deref().unwrap_or("default")
    }
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self::default_codex_fast()
    }
}

/// Normalize a model slug accepted by the CLI into a Responses model id.
pub fn normalize_model_slug(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with("gpt-") {
        trimmed.to_string()
    } else if trimmed.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        format!("gpt-{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Parse a CLI reasoning effort value.
///
/// Reasoning effort is provider-specific, so the harness does not validate
/// the set of accepted values. `default` disables the explicit reasoning
/// setting and `none` serializes as the literal `reasoning.effort` value.
pub fn parse_reasoning_effort(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace('-', "");
    if normalized.is_empty() || normalized == "default" {
        None
    } else {
        Some(normalized)
    }
}

/// Parse a CLI service tier value.
pub fn parse_service_tier(value: &str) -> Result<Option<String>, ResponsesModelSettingParseError> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "default" | "none" => Ok(None),
        "fast" | FAST_SERVICE_TIER => Ok(Some(FAST_SERVICE_TIER.to_string())),
        "flex" => Ok(Some("flex".to_string())),
        _ => Err(ResponsesModelSettingParseError::UnsupportedServiceTier {
            value: value.to_string(),
        }),
    }
}

#[derive(Debug, Serialize)]
struct ResponsesReasoning<'a> {
    effort: &'a str,
}

#[derive(Debug, Serialize)]
struct ResponsesCreateRequestBody<'a> {
    #[serde(rename = "type")]
    request_type: &'static str,
    model: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    instructions: &'a str,
    input: &'a [sonic_rs::Value],
    tools: Vec<sonic_rs::Value>,
    tool_choice: &'a str,
    parallel_tool_calls: bool,
    store: bool,
    stream: bool,
    include: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoning<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'a str>,
}

#[derive(Debug, Clone)]
/// Responses request plus headers and harness request id.
pub struct ResponsesRequest {
    /// Harness-assigned request id.
    pub request_id: ResponsesRequestId,
    /// Headers sent with the Responses request.
    pub headers: ResponsesHeaders,
    /// JSON body sent to the Responses API.
    pub body: sonic_rs::Value,
}

#[derive(Debug, Clone)]
/// Commands accepted by the Responses WebSocket actor.
pub enum ResponsesCommand {
    /// Send a v2 warmup request and keep the completed WebSocket open for reuse.
    Prewarm {
        /// Request used to warm the pool.
        request: ResponsesRequest,
        /// Event sink for actor events.
        events: ActorSender<ResponsesEvent>,
    },
    /// Send a request over an already pooled connection, opening one only when the pool is empty.
    Submit {
        /// Request to send.
        request: ResponsesRequest,
        /// Event sink for actor events.
        events: ActorSender<ResponsesEvent>,
    },
    /// Close idle pooled connections.
    CloseIdle,
    /// Shut down the Responses actor.
    Shutdown,
}

#[derive(Debug, Clone)]
/// Events emitted by the Responses WebSocket actor.
pub enum ResponsesEvent {
    /// A prewarm request completed.
    Prewarmed,
    /// A request started.
    RequestStarted(ResponsesRequestId),
    /// Server reported whether reasoning is included.
    ServerReasoningIncluded {
        /// Request that produced the event.
        request_id: ResponsesRequestId,
        /// Whether server reasoning is included.
        included: bool,
    },
    /// Server reported a models ETag.
    ModelsEtag {
        /// Request that produced the event.
        request_id: ResponsesRequestId,
        /// Models ETag value.
        etag: String,
    },
    /// Server reported the selected model.
    ServerModel {
        /// Request that produced the event.
        request_id: ResponsesRequestId,
        /// Server model name.
        model: String,
    },
    /// Final freeform/custom tool call from `response.output_item.*`.
    FreeformToolCall {
        /// Request that produced the tool call.
        request_id: ResponsesRequestId,
        /// Completed freeform tool call.
        call: FreeformToolCall,
    },
    /// Streaming raw input fragment from `response.custom_tool_call_input.delta`.
    FreeformToolInputDelta {
        /// Request that produced the delta.
        request_id: ResponsesRequestId,
        /// Freeform tool input delta.
        delta: FreeformToolInputDelta,
    },
    /// Final JSON/function tool call from `response.output_item.*`.
    FunctionToolCall {
        /// Request that produced the tool call.
        request_id: ResponsesRequestId,
        /// Completed function tool call.
        call: FunctionToolCall,
    },
    /// Streaming argument fragment from `response.function_call_arguments.delta`.
    FunctionToolInputDelta {
        /// Request that produced the delta.
        request_id: ResponsesRequestId,
        /// Function tool argument delta.
        delta: FunctionToolInputDelta,
    },
    /// Raw Responses stream frame.
    Frame {
        /// Request that produced the frame.
        request_id: ResponsesRequestId,
        /// Raw frame body.
        body: sonic_rs::Value,
    },
    /// Request completed.
    RequestCompleted(ResponsesRequestId),
    /// Request or actor error.
    Error {
        /// Request that failed, when known.
        request_id: Option<ResponsesRequestId>,
        /// Human-readable error message.
        message: String,
    },
    /// Responses actor shutdown completed.
    ShutdownComplete,
}

#[derive(Debug, Clone)]
enum ResponsesTransport {
    WebSocket(ResponsesWsPool),
    Https(ResponsesHttpsTransport),
}

#[derive(Debug)]
/// Actor that sends Responses requests over the configured provider transport.
pub struct ResponsesWsActor {
    transport: ResponsesTransport,
    rx: ActorReceiver<ResponsesCommand>,
}

impl ResponsesWsActor {
    /// Spawn a Responses actor backed by a WebSocket pool and return its command handle.
    pub fn spawn(pool: ResponsesWsPool) -> ActorHandle<ResponsesCommand> {
        Self::spawn_with_transport(ResponsesTransport::WebSocket(pool))
    }

    /// Spawn a Responses actor backed by HTTPS streaming transport.
    pub fn spawn_https(transport: ResponsesHttpsTransport) -> ActorHandle<ResponsesCommand> {
        Self::spawn_with_transport(ResponsesTransport::Https(transport))
    }

    fn spawn_with_transport(transport: ResponsesTransport) -> ActorHandle<ResponsesCommand> {
        let (handle, rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
        if let ResponsesTransport::WebSocket(pool) = &transport {
            pool.warm_idle_connections(ResponsesHeaders::for_thread(
                "responses-pool",
                "responses-pool",
                "responses-pool:idle",
            ));
        }
        let actor = Self { transport, rx };
        tokio::spawn(actor.run());
        handle
    }

    async fn run(self) {
        while let Ok(command) = self.rx.recv().await {
            match command {
                ResponsesCommand::Prewarm { request, events } => {
                    let transport = self.transport.clone();
                    tokio::spawn(async move {
                        let request_id = request.request_id;
                        let event = match transport {
                            ResponsesTransport::WebSocket(pool) => {
                                let stream_request = ResponsesStreamRequest {
                                    headers: request.headers,
                                    body: request.body,
                                };
                                match pool.prewarm(stream_request).await {
                                    Ok(()) => ResponsesEvent::Prewarmed,
                                    Err(err) => ResponsesEvent::Error {
                                        request_id: Some(request_id),
                                        message: err.describe(),
                                    },
                                }
                            }
                            ResponsesTransport::Https(_) => ResponsesEvent::Prewarmed,
                        };
                        let _ = events.send(event).await;
                    });
                }
                ResponsesCommand::Submit { request, events } => {
                    let transport = self.transport.clone();
                    tokio::spawn(async move {
                        let request_id = request.request_id;
                        let _ = events
                            .send(ResponsesEvent::RequestStarted(request_id))
                            .await;
                        match transport {
                            ResponsesTransport::WebSocket(pool) => {
                                let stream_request = ResponsesStreamRequest {
                                    headers: request.headers,
                                    body: request.body,
                                };
                                let result = pool
                                    .stream_request(stream_request, |event| {
                                        let events = events.clone();
                                        async move {
                                            send_stream_event(events, request_id, event).await;
                                        }
                                    })
                                    .await;
                                if let Err(err) = result {
                                    send_transport_error(&events, request_id, err).await;
                                }
                            }
                            ResponsesTransport::Https(transport) => {
                                if let Err(err) =
                                    transport.stream_request(request, events.clone()).await
                                {
                                    let _ = events
                                        .send(ResponsesEvent::Error {
                                            request_id: Some(request_id),
                                            message: err,
                                        })
                                        .await;
                                }
                            }
                        }
                    });
                }
                ResponsesCommand::CloseIdle => {
                    if let ResponsesTransport::WebSocket(pool) = &self.transport {
                        pool.close_idle().await;
                    }
                }
                ResponsesCommand::Shutdown => {
                    if let ResponsesTransport::WebSocket(pool) = &self.transport {
                        pool.close_idle().await;
                    }
                    break;
                }
            }
        }
    }
}

async fn send_stream_event(
    events: ActorSender<ResponsesEvent>,
    request_id: ResponsesRequestId,
    event: ResponsesStreamEvent,
) {
    let mapped = match event {
        ResponsesStreamEvent::ServerReasoningIncluded(included) => {
            ResponsesEvent::ServerReasoningIncluded {
                request_id,
                included,
            }
        }
        ResponsesStreamEvent::ModelsEtag(etag) => ResponsesEvent::ModelsEtag { request_id, etag },
        ResponsesStreamEvent::ServerModel(model) => {
            ResponsesEvent::ServerModel { request_id, model }
        }
        ResponsesStreamEvent::Frame(body) => {
            for event in native_tool_events_from_frame(request_id, &body) {
                let _ = events.send(event).await;
            }
            ResponsesEvent::Frame { request_id, body }
        }
        ResponsesStreamEvent::Completed { .. } => ResponsesEvent::RequestCompleted(request_id),
    };
    let _ = events.send(mapped).await;
}

async fn send_transport_error(
    events: &ActorSender<ResponsesEvent>,
    request_id: ResponsesRequestId,
    err: ResponsesApiError,
) {
    match err {
        ResponsesApiError::StreamInterrupted => {
            let _ = events
                .send(ResponsesEvent::RequestCompleted(request_id))
                .await;
        }
        err => {
            let _ = events
                .send(ResponsesEvent::Error {
                    request_id: Some(request_id),
                    message: err.describe(),
                })
                .await;
        }
    }
}

#[derive(Debug, Clone)]
/// HTTPS streaming transport for non-Codex Responses-compatible providers.
pub struct ResponsesHttpsTransport {
    provider: ApiProvider,
    auth: Auth,
    default_headers: HeaderMap,
    request_timeout: Duration,
    client: HyperClient<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl ResponsesHttpsTransport {
    /// Create an HTTPS streaming transport.
    pub fn new(
        provider: ApiProvider,
        auth: Auth,
        default_headers: HeaderMap,
        request_timeout: Duration,
    ) -> Self {
        let https = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            provider,
            auth,
            default_headers,
            request_timeout,
            client: HyperClient::builder(TokioExecutor::new()).build(https),
        }
    }

    async fn stream_request(
        &self,
        request: ResponsesRequest,
        events: ActorSender<ResponsesEvent>,
    ) -> Result<(), String> {
        let request_id = request.request_id;
        let body = sonic_rs::to_vec(&request.body).map_err(|err| err.to_string())?;
        let url = self.provider.endpoint_url(ApiEndpoint::Responses);
        let mut headers = self.default_headers.clone();
        self.auth
            .add_headers(&mut headers)
            .map_err(|err| err.to_string())?;
        headers.insert(
            CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        if !headers.contains_key(USER_AGENT) {
            headers.insert(
                USER_AGENT,
                http::HeaderValue::from_static(CODEX_CLI_USER_AGENT),
            );
        }

        let mut http_request = Request::post(url.as_str())
            .body(Full::new(Bytes::from(body)))
            .map_err(|err| err.to_string())?;
        http_request.headers_mut().extend(headers);

        let response =
            tokio::time::timeout(self.request_timeout, self.client.request(http_request))
                .await
                .map_err(|_| "HTTPS responses request timed out".to_string())?
                .map_err(|err| err.to_string())?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|err| err.to_string())?
            .to_bytes();

        if !status.is_success() {
            return Err(format!(
                "HTTP error {status}: {}",
                String::from_utf8_lossy(&body)
            ));
        }

        let text = String::from_utf8_lossy(&body);
        if text
            .lines()
            .any(|line| line.starts_with("data:") || line.starts_with(':'))
        {
            stream_sse_text(request_id, &events, &text).await?;
        } else if !text.trim().is_empty() {
            let frame = sonic_rs::from_str::<sonic_rs::Value>(text.trim())
                .map_err(|err| format!("failed to decode HTTPS response body: {err}"))?;
            emit_frame_event(request_id, &events, frame).await;
        }

        let _ = events
            .send(ResponsesEvent::RequestCompleted(request_id))
            .await;
        Ok(())
    }
}

async fn stream_sse_text(
    request_id: ResponsesRequestId,
    events: &ActorSender<ResponsesEvent>,
    text: &str,
) -> Result<(), String> {
    let mut data = String::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            if line.trim().is_empty() && !data.trim().is_empty() {
                emit_sse_data(request_id, events, &data).await?;
                data.clear();
            }
            continue;
        };
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(rest.trim_start());
    }
    if !data.trim().is_empty() {
        emit_sse_data(request_id, events, &data).await?;
    }
    Ok(())
}

async fn emit_sse_data(
    request_id: ResponsesRequestId,
    events: &ActorSender<ResponsesEvent>,
    data: &str,
) -> Result<(), String> {
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(());
    }
    let frame = sonic_rs::from_str::<sonic_rs::Value>(data)
        .map_err(|err| format!("failed to decode HTTPS stream frame: {err}; frame: {data}"))?;
    emit_frame_event(request_id, events, frame).await;
    Ok(())
}

async fn emit_frame_event(
    request_id: ResponsesRequestId,
    events: &ActorSender<ResponsesEvent>,
    frame: sonic_rs::Value,
) {
    for event in native_tool_events_from_frame(request_id, &frame) {
        let _ = events.send(event).await;
    }
    let _ = events
        .send(ResponsesEvent::Frame {
            request_id,
            body: frame,
        })
        .await;
}

fn native_tool_events_from_frame(
    request_id: ResponsesRequestId,
    body: &sonic_rs::Value,
) -> Vec<ResponsesEvent> {
    let mut events = Vec::new();
    match parse_freeform_tool_input_delta_from_frame(body) {
        Ok(Some(delta)) => {
            events.push(ResponsesEvent::FreeformToolInputDelta { request_id, delta })
        }
        Ok(None) => {}
        Err(err) => events.push(ResponsesEvent::Error {
            request_id: Some(request_id),
            message: err.to_string(),
        }),
    }
    match parse_freeform_tool_call_from_frame(body) {
        Ok(Some(call)) => events.push(ResponsesEvent::FreeformToolCall { request_id, call }),
        Ok(None) => {}
        Err(err) => events.push(ResponsesEvent::Error {
            request_id: Some(request_id),
            message: err.to_string(),
        }),
    }
    match parse_function_tool_input_delta_from_frame(body) {
        Ok(Some(delta)) => {
            events.push(ResponsesEvent::FunctionToolInputDelta { request_id, delta })
        }
        Ok(None) => {}
        Err(err) => events.push(ResponsesEvent::Error {
            request_id: Some(request_id),
            message: err.to_string(),
        }),
    }
    match parse_function_tool_call_from_frame(body) {
        Ok(Some(call)) => events.push(ResponsesEvent::FunctionToolCall { request_id, call }),
        Ok(None) => {}
        Err(err) => events.push(ResponsesEvent::Error {
            request_id: Some(request_id),
            message: err.to_string(),
        }),
    }
    events
}

#[cfg(test)]
mod tests {
    use sonic_rs::{JsonContainerTrait, JsonValueTrait, json};

    use super::*;
    use crate::tools::{APPLY_PATCH_LARK_GRAMMAR, FREEFORM_TOOL_TYPE};

    #[test]
    fn response_create_body_embeds_freeform_native_tools_only() {
        let body =
            ResponsesCreateRequest::new("gpt-test", ResponsesModelCapabilities::new(false, false))
                .with_instructions("Use native tools.")
                .with_input(vec![json!({
                    "type": "message",
                    "role": "user",
                    "content": "change a file"
                })])
                .with_tools(vec![NativeTool::apply_patch()])
                .to_body()
                .expect("serialize response.create body");

        let object = body.as_object().unwrap();
        assert_eq!(
            object.get(&"type").and_then(JsonValueTrait::as_str),
            Some("response.create")
        );
        assert_eq!(
            object.get(&"model").and_then(JsonValueTrait::as_str),
            Some("gpt-test")
        );
        assert_eq!(
            object.get(&"tool_choice").and_then(JsonValueTrait::as_str),
            Some("auto")
        );
        assert_eq!(
            object
                .get(&"parallel_tool_calls")
                .and_then(JsonValueTrait::as_bool),
            Some(false)
        );
        assert_eq!(
            object.get(&"store").and_then(JsonValueTrait::as_bool),
            Some(false)
        );

        let tools = object
            .get(&"tools")
            .and_then(|value| value.as_array())
            .unwrap();
        assert_eq!(tools.len(), 1);
        let tool = tools[0].as_object().unwrap();
        assert_eq!(
            tool.get(&"type").and_then(JsonValueTrait::as_str),
            Some(FREEFORM_TOOL_TYPE)
        );
        assert_eq!(
            tool.get(&"name").and_then(JsonValueTrait::as_str),
            Some("apply_patch")
        );
        assert_eq!(
            tool.get(&"format")
                .and_then(|value| value.as_object())
                .and_then(|format| format.get(&"definition"))
                .and_then(JsonValueTrait::as_str),
            Some(APPLY_PATCH_LARK_GRAMMAR)
        );
    }

    #[test]
    fn response_create_body_disables_backend_parallel_tool_calls() {
        let body =
            ResponsesCreateRequest::new("gpt-test", ResponsesModelCapabilities::new(true, false))
                .with_tools(vec![NativeTool::apply_patch()])
                .to_body()
                .expect("serialize response.create body");

        assert_eq!(
            body.as_object()
                .unwrap()
                .get(&"parallel_tool_calls")
                .and_then(JsonValueTrait::as_bool),
            Some(false)
        );
    }

    #[test]
    fn response_model_capabilities_come_from_queried_model_metadata() {
        let model_info = ResponsesModelInfo {
            slug: "gpt-test".to_string(),
            supports_parallel_tool_calls: true,
            supports_reasoning: true,
            context_window: Some(272_000),
            max_context_window: Some(1_000_000),
            auto_compact_token_limit: None,
            effective_context_window_percent: 95,
        };

        let capabilities = ResponsesModelCapabilities::from_model_info(&model_info);

        assert_eq!(capabilities.supports_parallel_tool_calls, true);
    }

    #[test]
    fn response_model_capabilities_override_gpt_5_5_parallel_tools() {
        let model_info = ResponsesModelInfo {
            slug: "gpt-5.5".to_string(),
            supports_parallel_tool_calls: false,
            supports_reasoning: true,
            context_window: Some(272_000),
            max_context_window: Some(1_000_000),
            auto_compact_token_limit: None,
            effective_context_window_percent: 95,
        };

        let capabilities = ResponsesModelCapabilities::from_model_info(&model_info);

        assert_eq!(capabilities.supports_parallel_tool_calls, true);
    }

    #[test]
    fn context_window_policy_comes_from_queried_model_metadata() {
        let model_info = ResponsesModelInfo {
            slug: "gpt-test".to_string(),
            supports_parallel_tool_calls: true,
            supports_reasoning: true,
            context_window: Some(272_000),
            max_context_window: Some(1_000_000),
            auto_compact_token_limit: None,
            effective_context_window_percent: 95,
        };

        let policy = context_window_policy_from_model_info(&model_info).unwrap();

        assert_eq!(policy.max_input_tokens, 258_400);
        assert_eq!(policy.compact_at_tokens, 245_480);
        assert_eq!(policy.target_tokens_after_compaction, 122_740);
    }

    #[test]
    fn context_window_policy_overrides_gpt_5_5_effective_window() {
        let model_info = ResponsesModelInfo {
            slug: "gpt-5.5".to_string(),
            supports_parallel_tool_calls: true,
            supports_reasoning: true,
            context_window: Some(272_000),
            max_context_window: Some(1_000_000),
            auto_compact_token_limit: Some(245_480),
            effective_context_window_percent: 95,
        };

        let policy = context_window_policy_from_model_info(&model_info).unwrap();

        assert_eq!(policy.max_input_tokens, 300_000);
        assert_eq!(policy.compact_at_tokens, 285_000);
        assert_eq!(policy.target_tokens_after_compaction, 200_000);
    }

    #[test]
    fn native_tool_events_from_frame_emits_custom_tool_input_delta() {
        let request_id = ResponsesRequestId(7);
        let events = native_tool_events_from_frame(
            request_id,
            &json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": "ctc-1",
                "call_id": "call-1",
                "delta": "*** Begin Patch\n",
            }),
        );

        assert_eq!(events.len(), 1);
        let ResponsesEvent::FreeformToolInputDelta { request_id, delta } = &events[0] else {
            panic!("expected freeform input delta event");
        };
        assert_eq!(*request_id, ResponsesRequestId(7));
        assert_eq!(delta.item_id, "ctc-1");
        assert_eq!(delta.call_id.as_deref(), Some("call-1"));
        assert_eq!(delta.delta, "*** Begin Patch\n");
    }

    #[test]
    fn native_tool_events_from_frame_emits_function_argument_delta() {
        let events = native_tool_events_from_frame(
            ResponsesRequestId(8),
            &json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc-1",
                "call_id": "call-function",
                "delta": "{\"cmd\":\"",
            }),
        );

        assert_eq!(events.len(), 1);
        let ResponsesEvent::FunctionToolInputDelta { request_id, delta } = &events[0] else {
            panic!("expected function argument delta event");
        };
        assert_eq!(*request_id, ResponsesRequestId(8));
        assert_eq!(delta.item_id, "fc-1");
        assert_eq!(delta.call_id.as_deref(), Some("call-function"));
        assert_eq!(delta.delta, "{\"cmd\":\"");
    }

    #[test]
    fn native_tool_events_from_frame_emits_function_tool_call() {
        let events = native_tool_events_from_frame(
            ResponsesRequestId(9),
            &json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": "call-function",
                    "name": "function_tool",
                    "arguments": "{\"cmd\":\"date\"}"
                }
            }),
        );

        assert_eq!(events.len(), 1);
        let ResponsesEvent::FunctionToolCall { request_id, call } = &events[0] else {
            panic!("expected function tool call event");
        };
        assert_eq!(*request_id, ResponsesRequestId(9));
        assert_eq!(call.call_id, "call-function");
        assert_eq!(call.name, "function_tool");
        assert_eq!(call.arguments, "{\"cmd\":\"date\"}");
    }

    #[tokio::test]
    async fn stream_sse_text_ignores_keepalive_ping_comment() {
        let (events, rx) = crate::actors::channel(10);
        let request_id = ResponsesRequestId(1);

        let result = stream_sse_text(request_id, &events.sender(), ": PING\n").await;
        assert!(result.is_ok());

        drop(events);
        assert!(rx.recv().await.is_err());
    }

    #[tokio::test]
    async fn stream_sse_text_processes_mixed_data_and_comments() {
        let (events, rx) = crate::actors::channel(10);
        let request_id = ResponsesRequestId(1);

        let result = stream_sse_text(
            request_id,
            &events.sender(),
            ": PING\ndata: {\"type\": \"response.output_item.done\", \"item\": {}}\n: PING\n",
        )
        .await;
        assert!(result.is_ok());

        drop(events);
        let mut got_event = false;
        while let Ok(event) = rx.recv().await {
            match event {
                ResponsesEvent::Frame { .. } => got_event = true,
                _ => {}
            }
        }
        assert!(got_event);
    }
}
