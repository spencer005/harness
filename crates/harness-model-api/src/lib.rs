//! Provider-neutral contracts for model requests, events, and terminal outcomes.

use std::{future::Future, pin::Pin, sync::Arc};

use harness_tool_api::ToolDefinition;
use thiserror::Error;

/// Identifier for a semantic model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelRequestId(pub u64);

/// Identifier for one attempt at a semantic model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelAttemptId(pub u64);

/// Identifier for a provider.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    /// Creates a provider identifier from a nonempty value.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidProviderId> {
        let value = value.into();
        if value.is_empty() {
            return Err(InvalidProviderId);
        }
        Ok(Self(value))
    }

    /// Returns the provider identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Failure returned when a provider identifier is empty.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("provider identifier is empty")]
pub struct InvalidProviderId;

/// Generation of a resolved provider instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderGeneration(pub u64);

/// Selected model and its provider-neutral settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    /// Provider identifier.
    pub provider: ProviderId,
    /// Model identifier.
    pub model: String,
    /// Optional reasoning effort.
    pub reasoning_effort: Option<String>,
    /// Optional service tier.
    pub service_tier: Option<String>,
}
impl ModelSelection {
    /// Creates a provider-neutral selection without applying provider policy.
    pub fn new(
        provider: ProviderId,
        model: impl Into<String>,
        reasoning_effort: Option<String>,
        service_tier: Option<String>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            reasoning_effort,
            service_tier,
        }
    }
}

/// Level of support for `developer` role in place of `system`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeveloperRoleSupport {
    /// Disallow developer role / use standard `system` role.
    #[default]
    Disabled,
    /// Support developer role (maps `system` messages to `developer`).
    Supported,
    /// Only allow developer role and disallow `system` role.
    DeveloperOnly,
}

/// Capabilities exposed by a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModelCapabilities {
    /// The model accepts tool definitions.
    pub tool_calls: bool,
    /// The model accepts freeform tool input.
    pub freeform_tool_input: bool,
    /// The model supports streaming.
    pub streaming: bool,
    /// Developer role support level.
    pub developer_role_support: DeveloperRoleSupport,
    /// Whether the model accepts multiple system/developer messages.
    pub allow_multiple_system_messages: bool,
    /// Whether responses should be stored by the provider (`store: true` vs `store: false`).
    pub store: bool,
}

/// Context-window limits for a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextLimits {
    /// Maximum input token count.
    pub max_input_tokens: u64,
    /// Compaction threshold.
    pub compact_at_tokens: u64,
}
impl ContextLimits {
    /// Creates valid nonzero context limits.
    pub fn new(
        max_input_tokens: u64,
        compact_at_tokens: u64,
    ) -> Result<Self, InvalidContextLimits> {
        if max_input_tokens == 0 || compact_at_tokens == 0 || compact_at_tokens > max_input_tokens {
            return Err(InvalidContextLimits);
        }
        Ok(Self {
            max_input_tokens,
            compact_at_tokens,
        })
    }
}

/// Failure returned for zero or inconsistent context limits.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("context limits must be nonzero and compaction must not exceed maximum input")]
pub struct InvalidContextLimits;

/// Provider-neutral canonical model input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelInput {
    /// A text message authored by a conversation participant.
    Message {
        /// Role of the message.
        role: ModelMessageRole,
        /// Exact message text.
        text: String,
    },
    /// An assistant request to invoke a function tool.
    AssistantToolCall {
        /// Provider call identity.
        call_id: String,
        /// Advertised tool name.
        name: String,
        /// Exact serialized JSON arguments.
        arguments: String,
    },
    /// An assistant request to invoke a native freeform/custom tool.
    FreeformToolCall {
        /// Provider call identity.
        call_id: String,
        /// Advertised tool name.
        name: String,
        /// Exact raw tool input.
        input: String,
    },
    /// Model-visible output correlated with one assistant tool call.
    ToolResult {
        /// Provider call identity.
        call_id: String,
        /// Exact model-visible output.
        output: String,
    },
    /// Result returned for a native freeform/custom tool call.
    FreeformToolResult {
        /// Provider call identity.
        call_id: String,
        /// Exact model-visible output.
        output: String,
    },
    /// Reasoning item retained for a subsequent Responses request.
    Reasoning {
        /// Raw reasoning content, when exposed.
        content: Option<String>,
        /// Opaque encrypted reasoning content.
        encrypted_content: Option<String>,
        /// Reasoning summary, when exposed.
        summary: Option<String>,
    },
}

/// Role used by provider-neutral text messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelMessageRole {
    /// System instruction.
    System,
    /// Developer instruction.
    Developer,
    /// User message.
    User,
    /// Assistant message.
    Assistant,
}

/// Locally estimated context usage for one immutable request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestContextUsage {
    /// Tokens in the finalized provider-neutral request.
    pub estimated_input_tokens: u64,
    /// Maximum input accepted by the selected model.
    pub max_input_tokens: u64,
    /// Threshold at which compaction should run.
    pub compact_at_tokens: u64,
}

/// Immutable semantic model request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequest {
    /// Semantic request identifier.
    pub request_id: ModelRequestId,
    /// Provider generation used to construct this request.
    /// Local context estimate when the selected model has a known tokenizer.
    pub context_usage: Option<RequestContextUsage>,
    pub provider_generation: ProviderGeneration,
    /// Exact canonical history revision used to build the request.
    pub history_revision: u64,
    /// Selected model.
    pub selection: ModelSelection,
    /// Provider-neutral input history.
    pub input: Arc<[ModelInput]>,
    /// Tool definitions included in this request.
    pub tools: Arc<[ToolDefinition]>,
    /// Optional previous response identifier for stateful response continuation.
    pub previous_response_id: Option<String>,
}

/// Reason a model request is attempted again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryReason {
    /// The transport requires a reconnect without changing semantic input.
    Reconnect,
    /// The provider asks for a retry after a transient failure.
    ProviderTransientFailure,
    /// The runtime retries after a supervised task interruption.
    SupervisedTaskInterrupted,
}

/// One attempt at an immutable semantic model request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAttempt {
    /// Immutable semantic request snapshot.
    pub request: Arc<ModelRequest>,
    /// Unique attempt identity.
    pub attempt_id: ModelAttemptId,
    /// Reason for this attempt, when it is a retry.
    pub retry_reason: Option<RetryReason>,
}

impl ModelAttempt {
    /// Creates the first attempt for a semantic request.
    pub fn initial(request: Arc<ModelRequest>, attempt_id: ModelAttemptId) -> Self {
        Self {
            request,
            attempt_id,
            retry_reason: None,
        }
    }

    /// Creates a retry using the same semantic request snapshot.
    pub fn retry(&self, attempt_id: ModelAttemptId, reason: RetryReason) -> Self {
        Self {
            request: Arc::clone(&self.request),
            attempt_id,
            retry_reason: Some(reason),
        }
    }
}

/// Route selected for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelRoute {
    /// Provider identity.
    pub provider: ProviderId,
    /// Provider generation that owns the route.
    pub generation: ProviderGeneration,
    /// Route name selected by the provider resolver.
    pub name: String,
    /// Model selection.
    pub selection: ModelSelection,
}
impl ResolvedModelRoute {
    /// Creates a route whose provider matches its model selection.
    pub fn new(
        provider: ProviderId,
        generation: ProviderGeneration,
        name: impl Into<String>,
        selection: ModelSelection,
    ) -> Result<Self, InvalidModelRoute> {
        let name = name.into();
        if name.is_empty() {
            return Err(InvalidModelRoute::EmptyName);
        }
        if selection.provider != provider {
            return Err(InvalidModelRoute::ProviderMismatch);
        }
        Ok(Self {
            provider,
            generation,
            name,
            selection,
        })
    }
}

/// Failure returned when a model route is internally inconsistent.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum InvalidModelRoute {
    /// Route name is empty.
    #[error("model route name is empty")]
    EmptyName,
    /// Route provider differs from the model selection provider.
    #[error("model route provider differs from model selection provider")]
    ProviderMismatch,
}

/// Usage observed from a model provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelUsage {
    /// Input token count, when supplied by the provider.
    pub input_tokens: Option<u64>,
    /// Output token count, when supplied by the provider.
    pub output_tokens: Option<u64>,
}

/// Completed model response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCompletion {
    /// Final assistant text.
    pub text: String,
    /// Observed usage, when supplied.
    pub usage: Option<ModelUsage>,
}

/// Reason a model stream is interrupted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInterruption {
    /// Human-readable interruption reason.
    pub reason: String,
}

/// Reason a model stream is cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCancellation {
    /// Human-readable cancellation reason.
    pub reason: String,
}

/// Failure category for model operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFailureKind {
    /// Authentication failure.
    Authentication,
    /// Provider rate limit.
    RateLimited,
    /// Context limit rejection.
    ContextLimit,
    /// Provider requires a reconnect.
    ReconnectRequired,
    /// Protocol decoding failure.
    Protocol,
    /// Network or connection failure.
    Transport,
    /// Request deadline expires.
    Timeout,
    /// Provider rejects the request.
    ProviderRejected,
}

/// Failed model operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFailure {
    /// Failure category.
    pub kind: ModelFailureKind,
    /// Provider failure detail.
    pub message: String,
}

/// Terminal model outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelTerminalOutcome {
    /// Model completes normally.
    Completed(ModelCompletion),
    /// Model stream is interrupted.
    Interrupted(ModelInterruption),
    /// Model stream is cancelled.
    Cancelled(ModelCancellation),
    /// Model operation fails.
    Failed(ModelFailure),
}

/// Reasoning item returned by a model response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelReasoning {
    /// Raw reasoning content, when the provider exposes it.
    pub content: Option<String>,
    /// Provider-defined encrypted reasoning payload.
    pub encrypted_content: Option<String>,
    /// Human-readable reasoning summary, when the provider exposes it.
    pub summary: Option<String>,
}

/// Incremental model event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    /// Model request starts.
    Started,
    /// Provider response metadata.
    Metadata(ModelResponseMetadata),
    /// Incremental assistant text.
    AssistantTextDelta(String),
    /// Incremental reasoning summary text.
    ReasoningSummaryDelta(String),
    /// Incremental raw reasoning content text.
    ReasoningContentDelta(String),
    /// Completed reasoning item metadata.
    ReasoningItem(ModelReasoning),
    /// Incremental tool input.
    ToolInputDelta(ToolInputDelta),
    /// Complete tool call.
    ToolCall(ToolCall),
    /// Observed usage.
    Usage(ModelUsage),
    /// Exactly one terminal event.
    Terminal(ModelTerminalOutcome),
}

/// Model response metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelResponseMetadata {
    /// Provider response identifier.
    pub response_id: Option<String>,
}

/// Incremental tool input fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInputDelta {
    /// Tool call identifier.
    pub call_id: String,
    /// Fragment text.
    pub fragment: String,
}

/// Complete tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Tool call identifier.
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// Tool input.
    pub input: harness_tool_api::ToolInput,
}

/// Terminal outcome emitted by a transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTransportOutcome {
    /// Request identifier.
    pub request_id: ModelRequestId,
    /// Attempt identifier.
    pub attempt_id: ModelAttemptId,
    /// Terminal outcome.
    pub outcome: ModelTerminalOutcome,
}

/// Model transport contract.
///
/// Implementations start a request and return a stream handle. The handle owns
/// cancellation and terminal completion for the attempt.
pub trait ModelTransport: Send + Sync {
    /// Starts one model attempt from an immutable request snapshot.
    fn start(
        &self,
        attempt: Arc<ModelAttempt>,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn ModelAttemptHandle>, ModelFailure>> + Send + '_>>;
    /// Cancels and joins every request owned by the transport.
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), ModelFailure>> + Send + '_>>;
}

/// Active model attempt contract.
pub trait ModelAttemptHandle: Send {
    /// Cancels this attempt.
    fn cancel(&mut self, reason: ModelCancellation);

    /// Returns the next event, or `None` after the event stream closes.
    fn next_event(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<ModelEvent>> + Send + '_>>;
}
