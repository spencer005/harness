//! Codex-compatible client primitives for the OpenAI Responses API.
//!
//! The crate contains the ChatGPT authentication bridge, Codex header
//! construction, `/models` metadata client, and the deterministic Responses
//! WebSocket transport pool used by the harness runtime.

#![deny(missing_docs)]

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use http::{
    HeaderMap, HeaderName, HeaderValue, Request, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE, ETAG, USER_AGENT},
};
use http_body_util::{BodyExt, Full};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client as HyperClient, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::{Deserialize, Serialize};
use sonic_rs::{JsonContainerTrait, JsonValueMutTrait, JsonValueTrait, Object, Value};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Error as WsError;
use url::Url;

/// Header name for selecting OpenAI beta protocol contracts.
pub const OPENAI_BETA_HEADER: &str = "openai-beta";
/// Beta contract value for Responses WebSockets v2.
pub const OPENAI_BETA_RESPONSES_WEBSOCKETS_V2: &str = "responses_websockets=2026-02-06";
/// Header name for the Codex client request identifier.
pub const X_CLIENT_REQUEST_ID: &str = "x-client-request-id";
/// Header name for the logical Codex session identifier.
pub const SESSION_ID: &str = "session-id";
/// Header name for the logical Codex thread identifier.
pub const THREAD_ID: &str = "thread-id";
/// Header name for Codex beta feature flags.
pub const X_CODEX_BETA_FEATURES: &str = "x-codex-beta-features";
/// Client metadata key for the Codex installation identifier.
pub const X_CODEX_INSTALLATION_ID: &str = "x-codex-installation-id";
/// Header and client metadata key for the parent Codex thread identifier.
pub const X_CODEX_PARENT_THREAD_ID: &str = "x-codex-parent-thread-id";
/// Header and client metadata key for opaque Codex turn metadata.
pub const X_CODEX_TURN_METADATA: &str = "x-codex-turn-metadata";
/// Header name used to carry server-provided continuation state between turns.
pub const X_CODEX_TURN_STATE: &str = "x-codex-turn-state";
/// Header and client metadata key for the Codex window identifier.
pub const X_CODEX_WINDOW_ID: &str = "x-codex-window-id";
/// Client metadata key containing the request send timestamp in Unix milliseconds.
pub const X_CODEX_WS_STREAM_REQUEST_START_MS: &str = "x-codex-ws-stream-request-start-ms";
/// Header name for the OpenAI attestation token.
pub const X_OAI_ATTESTATION: &str = "x-oai-attestation";
/// Header name marking FedRAMP ChatGPT accounts.
pub const X_OPENAI_FEDRAMP: &str = "x-openai-fedramp";
/// Header and client metadata key for Codex subagent identity.
pub const X_OPENAI_SUBAGENT: &str = "x-openai-subagent";
/// Handshake response header indicating server-side reasoning inclusion.
pub const X_REASONING_INCLUDED: &str = "x-reasoning-included";
/// Handshake response header containing the `/models` catalog `ETag`.
pub const X_MODELS_ETAG: &str = "x-models-etag";
/// Handshake response header containing the server-selected model slug.
pub const OPENAI_MODEL: &str = "openai-model";
/// Header name requesting Responses API timing metrics.
pub const X_RESPONSESAPI_INCLUDE_TIMING_METRICS: &str = "x-responsesapi-include-timing-metrics";
/// Header name for the selected ChatGPT account identifier.
pub const CHATGPT_ACCOUNT_ID: &str = "chatgpt-account-id";
/// Header name for the Codex originator identity.
pub const ORIGINATOR: &str = "originator";
/// Codex `originator` header value used by the OpenAI Codex CLI.
pub const DEFAULT_CODEX_ORIGINATOR: &str = "codex_cli_rs";
/// Codex CLI version mirrored in client identity mechanisms.
///
/// The backend treats `0.0.0` as the `git` client version. This harness
/// reports that value intentionally so model and routing behavior matches the
/// unversioned Codex client path rather than a pinned released build.
pub const CODEX_CLI_CLIENT_VERSION: &str = "0.0.0";
/// Static Codex-like `User-Agent` sent by this harness.
///
/// Stock Codex formats this as
/// `originator/version (os version; arch) terminal`. This harness keeps the
/// same shape with static Linux metadata instead of adding Codex's OS and
/// terminal-detection dependencies.
pub const CODEX_CLI_USER_AGENT: &str = "codex_cli_rs/0.0.0 (Linux 6.8.0; x86_64) unknown";
/// OAuth client identifier used by the Codex ChatGPT auth flow.
pub const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Default ChatGPT refresh-token endpoint used by Codex auth.
pub const CHATGPT_REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Environment variable that overrides the ChatGPT refresh-token endpoint.
pub const CODEX_REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";

const RESPONSES_PATH: &str = "responses";
const MODELS_PATH: &str = "models";
const ANALYTICS_EVENTS_PATH: &str = "analytics-events/events";
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE: &str = "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue.";

type ApiHttpClient = HyperClient<HttpsConnector<HttpConnector>, Full<Bytes>>;
type RefreshHttpClient = ApiHttpClient;
/// Future returned by ChatGPT auth sessions while refreshing expired credentials.
pub type AuthRefreshFuture<'a> = Pin<Box<dyn Future<Output = Result<(), AuthError>> + Send + 'a>>;

fn boxed_error(
    error: impl std::error::Error + Send + Sync + 'static,
) -> Box<dyn std::error::Error + Send + Sync + 'static> {
    Box::new(error)
}

/// Construct a protocol error with a message.
pub fn protocol_error(message: impl Into<String>) -> ResponsesApiError {
    ResponsesApiError::Protocol {
        message: message.into(),
        source: None,
    }
}

/// Construct a protocol error with a message and source error.
pub fn protocol_source_error(
    message: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ResponsesApiError {
    ResponsesApiError::Protocol {
        message: message.into(),
        source: Some(boxed_error(source)),
    }
}

/// Construct a WebSocket error with a message.
pub fn websocket_error(message: impl Into<String>) -> ResponsesApiError {
    ResponsesApiError::WebSocket {
        message: message.into(),
        source: None,
    }
}

/// Construct a WebSocket error with a message and source error.
pub fn websocket_source_error(
    message: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ResponsesApiError {
    ResponsesApiError::WebSocket {
        message: message.into(),
        source: Some(boxed_error(source)),
    }
}

fn transport_error(source: impl std::error::Error + Send + Sync + 'static) -> ResponsesApiError {
    ResponsesApiError::Transport {
        source: boxed_error(source),
    }
}

/// Authentication and credential-refresh failures.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Stored authentication material could not be loaded.
    #[error("auth load failed: {message}")]
    Load {
        /// Human-readable load failure.
        message: String,
        /// Source error when the failure came from another error type.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },
    /// Required authentication material is absent.
    #[error("missing auth material: {0}")]
    Missing(&'static str),
    /// Authentication header value cannot be represented as an HTTP header.
    #[error("invalid auth header `{name}`")]
    InvalidHeader {
        /// Header name that received an invalid value.
        name: &'static str,
    },
    /// Credential refresh failed.
    #[error("auth refresh failed: {message}")]
    Refresh {
        /// Human-readable refresh failure.
        message: String,
        /// Source error when the failure came from another error type.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },
}

impl AuthError {
    /// Construct an auth-load error without a source error.
    pub fn load(message: impl Into<String>) -> Self {
        Self::Load {
            message: message.into(),
            source: None,
        }
    }

    /// Construct an auth-load error with a source error.
    pub fn load_with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Load {
            message: message.into(),
            source: Some(boxed_error(source)),
        }
    }

    /// Construct an auth-refresh error without a source error.
    pub fn refresh(message: impl Into<String>) -> Self {
        Self::Refresh {
            message: message.into(),
            source: None,
        }
    }

    /// Construct an auth-refresh error with a source error.
    pub fn refresh_with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Refresh {
            message: message.into(),
            source: Some(boxed_error(source)),
        }
    }
}

/// ChatGPT authentication session capable of providing and refreshing tokens.
pub trait ChatGptAuthSession: Send + Sync {
    /// Return the current access token.
    fn access_token(&self) -> Result<String, AuthError>;
    /// Return the selected ChatGPT account id when one is configured.
    fn account_id(&self) -> Result<Option<String>, AuthError>;

    /// Return whether the session belongs to a FedRAMP ChatGPT account.
    fn is_fedramp(&self) -> bool {
        false
    }

    /// Refresh credentials after the backend rejects a request as unauthorized.
    fn refresh_after_unauthorized(&self) -> AuthRefreshFuture<'_>;
}

/// ChatGPT OAuth token material used by the managed auth session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptAuthTokens {
    /// Bearer access token sent to the Responses API.
    pub access_token: String,
    /// Refresh token used to rotate access credentials.
    pub refresh_token: String,
    /// ChatGPT account id selected for requests.
    pub account_id: Option<String>,
    /// Whether this token set targets a FedRAMP account.
    pub fedramp: bool,
}

/// Managed ChatGPT auth session with serialized refresh-token rotation.
#[derive(Debug)]
pub struct ManagedChatGptAuth {
    tokens: RwLock<ChatGptAuthTokens>,
    refresh_lock: Mutex<()>,
    refresh_url: String,
    client: RefreshHttpClient,
}

impl ManagedChatGptAuth {
    /// Create a managed auth session using the default Codex refresh endpoint.
    pub fn new(tokens: ChatGptAuthTokens) -> Self {
        Self::with_refresh_url(tokens, refresh_token_endpoint())
    }

    /// Create a managed auth session using an explicit refresh endpoint.
    pub fn with_refresh_url(tokens: ChatGptAuthTokens, refresh_url: impl Into<String>) -> Self {
        Self {
            tokens: RwLock::new(tokens),
            refresh_lock: Mutex::new(()),
            refresh_url: refresh_url.into(),
            client: refresh_http_client(),
        }
    }

    /// Return a clone of the current token material.
    pub fn snapshot(&self) -> Result<ChatGptAuthTokens, AuthError> {
        self.tokens
            .read()
            .map_err(|_| AuthError::refresh("ChatGPT auth lock poisoned"))
            .map(|tokens| tokens.clone())
    }

    async fn refresh_tokens(&self) -> Result<(), AuthError> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let refresh_token = self.snapshot()?.refresh_token;
        if refresh_token.is_empty() {
            return Err(AuthError::Missing("refresh_token"));
        }

        let request = RefreshRequest {
            client_id: CODEX_OAUTH_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        };
        let body = sonic_rs::to_vec(&request).map_err(|err| {
            AuthError::refresh_with_source("failed to encode refresh request", err)
        })?;

        let request = Request::post(self.refresh_url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|err| {
                AuthError::refresh_with_source("failed to build refresh request", err)
            })?;
        let response =
            self.client.request(request).await.map_err(|err| {
                AuthError::refresh_with_source("failed to send refresh request", err)
            })?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|err| AuthError::refresh_with_source("failed to read refresh response", err))?
            .to_bytes();

        if !status.is_success() {
            return Err(AuthError::refresh(refresh_error_message(status, &body)));
        }

        let refresh = sonic_rs::from_slice::<RefreshResponse>(&body).map_err(|err| {
            AuthError::refresh_with_source("failed to decode refresh response", err)
        })?;
        let mut tokens = self
            .tokens
            .write()
            .map_err(|_| AuthError::refresh("ChatGPT auth lock poisoned"))?;
        if let Some(access_token) = refresh.access_token {
            tokens.access_token = access_token;
        }
        if let Some(refresh_token) = refresh.refresh_token {
            tokens.refresh_token = refresh_token;
        }
        Ok(())
    }
}

impl ChatGptAuthSession for ManagedChatGptAuth {
    fn access_token(&self) -> Result<String, AuthError> {
        Ok(self.snapshot()?.access_token)
    }

    fn account_id(&self) -> Result<Option<String>, AuthError> {
        Ok(self.snapshot()?.account_id)
    }

    fn is_fedramp(&self) -> bool {
        self.snapshot().is_ok_and(|tokens| tokens.fedramp)
    }

    fn refresh_after_unauthorized(&self) -> AuthRefreshFuture<'_> {
        Box::pin(self.refresh_tokens())
    }
}

#[derive(Debug, Serialize)]
struct RefreshRequest {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// Authentication mode used by Responses API clients.
#[derive(Clone)]
pub enum Auth {
    /// Static API key bearer token.
    ApiKey(String),
    /// ChatGPT OAuth session with refresh support.
    ChatGpt(Arc<dyn ChatGptAuthSession>),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(_) => f.debug_tuple("ApiKey").finish(),
            Self::ChatGpt(_) => f.debug_tuple("ChatGpt").finish(),
        }
    }
}

impl Auth {
    /// Return the current access token for header and analytics use.
    pub fn access_token(&self) -> Result<String, AuthError> {
        match self {
            Self::ApiKey(token) => Ok(token.clone()),
            Self::ChatGpt(session) => session.access_token(),
        }
    }

    /// Return whether this auth uses a ChatGPT OAuth session.
    pub fn is_chatgpt(&self) -> bool {
        matches!(self, Self::ChatGpt(_))
    }

    /// Add authentication headers to a request header map.
    pub fn add_headers(&self, headers: &mut HeaderMap) -> Result<(), AuthError> {
        match self {
            Self::ApiKey(token) => insert_bearer(headers, token),
            Self::ChatGpt(session) => {
                let token = session.access_token()?;
                insert_bearer(headers, &token)?;
                if let Some(account_id) = session.account_id()? {
                    insert_header(headers, CHATGPT_ACCOUNT_ID, &account_id).map_err(|_| {
                        AuthError::InvalidHeader {
                            name: CHATGPT_ACCOUNT_ID,
                        }
                    })?;
                }
                if session.is_fedramp() {
                    headers.insert(
                        HeaderName::from_static(X_OPENAI_FEDRAMP),
                        HeaderValue::from_static("true"),
                    );
                }
                Ok(())
            }
        }
    }

    /// Refreshes ChatGPT credentials after receiving an unauthorized response.
    pub async fn refresh_after_unauthorized(&self) -> Result<bool, AuthError> {
        match self {
            Self::ApiKey(_) => Ok(false),
            Self::ChatGpt(session) => {
                session.refresh_after_unauthorized().await?;
                Ok(true)
            }
        }
    }
}

fn insert_bearer(headers: &mut HeaderMap, token: &str) -> Result<(), AuthError> {
    let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        AuthError::InvalidHeader {
            name: "authorization",
        }
    })?;
    headers.insert(AUTHORIZATION, value);
    Ok(())
}

/// Captured turn-state header returned by a Responses WebSocket handshake.
#[derive(Debug, Clone, Default)]
pub struct ResponsesTurnState {
    value: Arc<OnceLock<String>>,
}

impl ResponsesTurnState {
    /// Create an empty turn-state capture slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the captured turn-state value when a handshake has provided one.
    pub fn get(&self) -> Option<&str> {
        self.value.get().map(String::as_str)
    }

    /// Captured turn-state value from a handshake.
    pub fn capture(&self, value: String) {
        let _ = self.value.set(value);
    }
}

/// Codex-compatible headers and metadata for a Responses WebSocket request.
#[derive(Debug, Clone)]
pub struct CodexHeaders {
    /// Request id sent as `x-client-request-id`.
    pub client_request_id: String,
    /// Logical Codex session id.
    pub session_id: String,
    /// Logical Codex thread id.
    pub thread_id: String,
    /// Codex window id for the request.
    pub window_id: String,
    /// Optional Codex installation id stamped into client metadata.
    pub installation_id: Option<String>,
    /// Optional parent thread id sent in headers and client metadata.
    pub parent_thread_id: Option<String>,
    /// Optional Codex subagent identity sent in headers and client metadata.
    pub subagent: Option<String>,
    /// Optional Codex beta feature string.
    pub beta_features: Option<String>,
    /// Optional shared slot for turn-state capture and replay.
    pub turn_state: Option<ResponsesTurnState>,
    /// Optional opaque turn metadata sent in headers and client metadata.
    pub turn_metadata: Option<String>,
    /// Optional OpenAI attestation token.
    pub attestation: Option<String>,
    /// Whether timing metric headers are requested from the backend.
    pub include_timing_metrics: bool,
}

impl CodexHeaders {
    /// Build the required Codex headers for one logical thread.
    pub fn for_thread(
        session_id: impl Into<String>,
        thread_id: impl Into<String>,
        window_id: impl Into<String>,
    ) -> Self {
        let thread_id = thread_id.into();
        Self {
            client_request_id: thread_id.clone(),
            session_id: session_id.into(),
            thread_id,
            window_id: window_id.into(),
            installation_id: None,
            parent_thread_id: None,
            subagent: None,
            beta_features: None,
            turn_state: None,
            turn_metadata: None,
            attestation: None,
            include_timing_metrics: false,
        }
    }

    /// Convert this logical header set into an HTTP header map for handshakes.
    pub fn to_header_map(&self) -> Result<HeaderMap, ResponsesApiError> {
        let mut headers = HeaderMap::new();
        insert_header(&mut headers, X_CLIENT_REQUEST_ID, &self.client_request_id)?;
        insert_header(&mut headers, SESSION_ID, &self.session_id)?;
        insert_header(&mut headers, THREAD_ID, &self.thread_id)?;
        insert_header(&mut headers, X_CODEX_WINDOW_ID, &self.window_id)?;
        headers.insert(
            HeaderName::from_static(OPENAI_BETA_HEADER),
            HeaderValue::from_static(OPENAI_BETA_RESPONSES_WEBSOCKETS_V2),
        );

        if let Some(value) = self
            .beta_features
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            insert_header(&mut headers, X_CODEX_BETA_FEATURES, value)?;
        }
        if let Some(turn_state) = &self.turn_state
            && let Some(value) = turn_state.get()
        {
            insert_header(&mut headers, X_CODEX_TURN_STATE, value)?;
        }
        if let Some(value) = &self.turn_metadata {
            insert_header(&mut headers, X_CODEX_TURN_METADATA, value)?;
        }
        if let Some(value) = &self.parent_thread_id {
            insert_header(&mut headers, X_CODEX_PARENT_THREAD_ID, value)?;
        }
        if let Some(value) = &self.subagent {
            insert_header(&mut headers, X_OPENAI_SUBAGENT, value)?;
        }
        if let Some(value) = &self.attestation {
            insert_header(&mut headers, X_OAI_ATTESTATION, value)?;
        }
        if self.include_timing_metrics {
            headers.insert(
                HeaderName::from_static(X_RESPONSESAPI_INCLUDE_TIMING_METRICS),
                HeaderValue::from_static("true"),
            );
        }
        Ok(headers)
    }

    /// Return per-request metadata stamped into the WebSocket JSON body.
    pub fn client_metadata(&self) -> HashMap<String, String> {
        let mut metadata = HashMap::new();
        metadata.insert(X_CODEX_WINDOW_ID.to_string(), self.window_id.clone());
        if let Some(installation_id) = &self.installation_id {
            metadata.insert(X_CODEX_INSTALLATION_ID.to_string(), installation_id.clone());
        }
        if let Some(subagent) = &self.subagent {
            metadata.insert(X_OPENAI_SUBAGENT.to_string(), subagent.clone());
        }
        if let Some(parent_thread_id) = &self.parent_thread_id {
            metadata.insert(
                X_CODEX_PARENT_THREAD_ID.to_string(),
                parent_thread_id.clone(),
            );
        }
        if let Some(turn_metadata) = &self.turn_metadata {
            metadata.insert(X_CODEX_TURN_METADATA.to_string(), turn_metadata.clone());
        }
        metadata
    }

    /// Stamp per-request Codex metadata into a `response.create` JSON body.
    pub fn stamp_client_metadata(&self, body: &mut Value) -> Result<(), ResponsesApiError> {
        let root = body
            .as_object_mut()
            .ok_or_else(|| protocol_error("Responses WebSocket frame must be a JSON object"))?;
        if let Some(existing) = root.get(&"client_metadata")
            && existing.as_object().is_none()
        {
            return Err(protocol_error(
                "Responses WebSocket frame client_metadata must be a JSON object",
            ));
        }
        if !root.contains_key(&"client_metadata") {
            root.insert(&"client_metadata", Object::new());
        }
        let metadata = root
            .get_mut(&"client_metadata")
            .and_then(|value| value.as_object_mut())
            .ok_or_else(|| {
                protocol_error("Responses WebSocket frame client_metadata must be a JSON object")
            })?;
        for (key, value) in self.client_metadata() {
            metadata.insert(&key, value.as_str());
        }
        let request_start_ms = now_unix_timestamp_ms().to_string();
        metadata.insert(
            &X_CODEX_WS_STREAM_REQUEST_START_MS,
            request_start_ms.as_str(),
        );
        Ok(())
    }
}

/// API endpoint exposed by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiEndpoint {
    /// Codex-compatible model metadata endpoint.
    Models,
    /// Responses streaming endpoint.
    Responses,
    /// Codex analytics event ingestion endpoint.
    AnalyticsEvents,
}

impl ApiEndpoint {
    /// Return the endpoint path relative to the provider base URL.
    pub fn relative_path(self) -> &'static str {
        match self {
            Self::Models => MODELS_PATH,
            Self::Responses => RESPONSES_PATH,
            Self::AnalyticsEvents => ANALYTICS_EVENTS_PATH,
        }
    }
}

/// API provider configuration shared by HTTP, WebSocket, and analytics clients.
///
/// The configured base URL owns every provider-specific path segment. Endpoints
/// are appended directly to that base; no API version segment is inferred.
#[derive(Debug, Clone)]
pub struct ApiProvider {
    base_url: Url,
    headers: HeaderMap,
    stream_idle_timeout: Duration,
}

impl ApiProvider {
    /// Create provider configuration with default timeouts and no headers.
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, ResponsesApiError> {
        let mut base_url =
            Url::parse(base_url.as_ref()).map_err(|source| ResponsesApiError::Url { source })?;
        validate_provider_base_url(&base_url)?;
        normalize_provider_base_url(&mut base_url);
        Ok(Self {
            base_url,
            headers: HeaderMap::new(),
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
        })
    }

    /// Return provider-specific headers applied before request and default headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Return the maximum idle interval while waiting for stream frames.
    pub fn stream_idle_timeout(&self) -> Duration {
        self.stream_idle_timeout
    }

    /// Replace provider-specific headers.
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// Replace the stream idle timeout.
    pub fn with_stream_idle_timeout(mut self, stream_idle_timeout: Duration) -> Self {
        self.stream_idle_timeout = stream_idle_timeout;
        self
    }

    /// Return the URL for a provider endpoint.
    pub fn endpoint_url(&self, endpoint: ApiEndpoint) -> Url {
        self.base_url
            .join(endpoint.relative_path())
            .expect("static provider endpoint paths are valid relative URL paths")
    }

    /// Return the WebSocket URL for a provider endpoint.
    pub fn websocket_endpoint_url(&self, endpoint: ApiEndpoint) -> Url {
        let mut url = self.endpoint_url(endpoint);
        let Some(scheme) = websocket_scheme(url.scheme()) else {
            unreachable!("provider scheme is validated during construction");
        };
        url.set_scheme(scheme)
            .expect("static websocket schemes are valid for provider URLs");
        url
    }

    /// Return a URL rooted at the provider origin.
    pub fn origin_url_for_path(&self, path: &str) -> Result<Url, ResponsesApiError> {
        let mut origin = self.base_url.clone();
        origin.set_path("/");
        origin
            .join(path)
            .map_err(|source| ResponsesApiError::Url { source })
    }
}

/// Minimal model metadata consumed by the native-tool request builder.
///
/// Provider model metadata consumed by the native-tool request builder and
/// provider runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesModelInfo {
    /// Model slug used in `response.create.model`.
    pub slug: String,
    /// Whether the model supports parallel tool calls.
    pub supports_parallel_tool_calls: bool,
    /// Whether the model supports reasoning parameters.
    pub supports_reasoning: bool,
    /// Model context window in input tokens when returned by `/models`.
    pub context_window: Option<u64>,
    /// Maximum context window available for the model.
    pub max_context_window: Option<u64>,
    /// Explicit automatic-compaction threshold in tokens.
    pub auto_compact_token_limit: Option<u64>,
    /// Percentage of the resolved context window considered usable for inputs.
    pub effective_context_window_percent: u64,
}

#[derive(Debug, Deserialize)]
struct ResponsesModelInfoWire {
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, alias = "supportsParallelToolCalls")]
    #[serde(alias = "supports_parallel_tools")]
    #[serde(alias = "supportsParallelTools")]
    supports_parallel_tool_calls: bool,
    #[serde(default, alias = "supportsReasoning")]
    #[serde(alias = "supports_thinking")]
    #[serde(alias = "supportsThinking")]
    supports_reasoning: bool,
    #[serde(default, alias = "contextWindow")]
    context_window: Option<u64>,
    #[serde(default, alias = "maxContextWindow")]
    max_context_window: Option<u64>,
    #[serde(default, alias = "autoCompactTokenLimit")]
    auto_compact_token_limit: Option<u64>,
    #[serde(
        default = "default_effective_context_window_percent",
        alias = "effectiveContextWindowPercent"
    )]
    effective_context_window_percent: u64,
}

impl<'de> Deserialize<'de> for ResponsesModelInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ResponsesModelInfoWire::deserialize(deserializer)?;
        let slug = wire
            .slug
            .or(wire.id)
            .filter(|slug| !slug.is_empty())
            .ok_or_else(|| serde::de::Error::missing_field("slug or id"))?;
        Ok(Self {
            slug,
            supports_parallel_tool_calls: wire.supports_parallel_tool_calls,
            supports_reasoning: wire.supports_reasoning,
            context_window: wire.context_window,
            max_context_window: wire.max_context_window,
            auto_compact_token_limit: wire.auto_compact_token_limit,
            effective_context_window_percent: wire.effective_context_window_percent,
        })
    }
}

impl ResponsesModelInfo {
    /// Return the model context window used for runtime planning.
    pub fn resolved_context_window(&self) -> Option<u64> {
        self.context_window.or(self.max_context_window)
    }

    /// Return the usable model input window after applying server metadata.
    pub fn effective_context_window(&self) -> Option<u64> {
        self.resolved_context_window().map(|context_window| {
            context_window.saturating_mul(self.effective_context_window_percent) / 100
        })
    }

    /// Return the token threshold that triggers automatic compaction.
    pub fn auto_compact_token_limit(&self) -> Option<u64> {
        let context_limit = self
            .effective_context_window()
            .map(|effective_context_window| (effective_context_window * 95) / 100);
        match (context_limit, self.auto_compact_token_limit) {
            (Some(context_limit), Some(config_limit)) => Some(config_limit.min(context_limit)),
            (Some(context_limit), None) => Some(context_limit),
            (None, Some(config_limit)) => Some(config_limit),
            (None, None) => None,
        }
    }
}

fn default_effective_context_window_percent() -> u64 {
    95
}

#[derive(Debug, Deserialize)]
struct ResponsesModelsResponse {
    #[serde(alias = "data")]
    models: Vec<ResponsesModelInfo>,
}

/// Result from the Codex-compatible `/models` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesModelsList {
    /// Model metadata returned by the backend.
    pub models: Vec<ResponsesModelInfo>,
    /// Optional `ETag` header returned by `/models`.
    pub etag: Option<String>,
}

impl ResponsesModelsList {
    /// Return metadata for a model slug when present in the queried catalog.
    pub fn model(&self, slug: &str) -> Option<&ResponsesModelInfo> {
        self.models.iter().find(|model| model.slug == slug)
    }
}

/// Request body for Ollama's native `/api/show` model metadata endpoint.
#[derive(Debug, Serialize)]
struct OllamaShowRequest<'a> {
    model: &'a str,
}

/// Response body from Ollama's native `/api/show` model metadata endpoint.
#[derive(Debug, Deserialize)]
struct OllamaShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    model_info: HashMap<String, Value>,
}

/// Response body from Ollama's native `/api/tags` model catalog endpoint.
#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagModel {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// Client for Ollama's native model metadata endpoints.
///
/// Ollama's OpenAI-compatible `/v1/models` response lists model ids but does not
/// include native capability data. `/api/tags` lists the upstream Ollama catalog,
/// and `/api/show` returns per-model parameters and capabilities such as
/// `thinking`.
#[derive(Clone)]
pub struct OllamaModelInfoClient {
    provider: ApiProvider,
    auth: Auth,
    default_headers: HeaderMap,
    client: ApiHttpClient,
}

impl OllamaModelInfoClient {
    /// Create a client for the Ollama native model metadata endpoint.
    pub fn new(provider: ApiProvider, auth: Auth, default_headers: HeaderMap) -> Self {
        Self {
            provider,
            auth,
            default_headers,
            client: api_http_client(),
        }
    }

    /// Query the native Ollama model catalog.
    pub async fn list_models(&self) -> Result<ResponsesModelsList, ResponsesApiError> {
        let url = self.provider.origin_url_for_path("api/tags")?;
        let request_headers = self.request_headers()?;
        let mut request = Request::get(url.as_str())
            .body(Full::new(Bytes::new()))
            .map_err(|err| protocol_source_error("failed to build request", err))?;
        request.headers_mut().extend(request_headers);

        let body = self.send(request).await?;
        let response = sonic_rs::from_slice::<OllamaTagsResponse>(&body).map_err(|err| {
            protocol_error(format!(
                "failed to decode Ollama model tags response: {err}; body: {}",
                String::from_utf8_lossy(&body)
            ))
        })?;
        let models = response
            .models
            .into_iter()
            .filter_map(|model| {
                model
                    .model
                    .or(model.name)
                    .filter(|slug| !slug.trim().is_empty())
                    .map(|slug| ResponsesModelInfo {
                        slug,
                        supports_parallel_tool_calls: false,
                        supports_reasoning: false,
                        context_window: None,
                        max_context_window: None,
                        auto_compact_token_limit: None,
                        effective_context_window_percent: default_effective_context_window_percent(
                        ),
                    })
            })
            .collect();

        Ok(ResponsesModelsList { models, etag: None })
    }

    /// Query the native Ollama metadata for `model`.
    pub async fn model_info(
        &self,
        model: &str,
    ) -> Result<Option<ResponsesModelInfo>, ResponsesApiError> {
        let body = sonic_rs::to_vec(&OllamaShowRequest { model }).map_err(|err| {
            protocol_source_error("failed to encode Ollama model info request", err)
        })?;
        let url = self.provider.origin_url_for_path("api/show")?;
        let mut request = Request::post(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|err| protocol_source_error("failed to build request", err))?;
        request.headers_mut().extend(self.request_headers()?);

        let body = self.send(request).await?;
        let response = sonic_rs::from_slice::<OllamaShowResponse>(&body).map_err(|err| {
            protocol_error(format!(
                "failed to decode Ollama model info response: {err}; body: {}",
                String::from_utf8_lossy(&body)
            ))
        })?;
        let supports_reasoning = ollama_supports_reasoning(&response.capabilities);
        let Some(context_window) = ollama_context_window(&response.model_info) else {
            return Ok(Some(ResponsesModelInfo {
                slug: model.to_string(),
                supports_parallel_tool_calls: false,
                supports_reasoning,
                context_window: None,
                max_context_window: None,
                auto_compact_token_limit: None,
                effective_context_window_percent: default_effective_context_window_percent(),
            }));
        };

        Ok(Some(ResponsesModelInfo {
            slug: model.to_string(),
            supports_parallel_tool_calls: false,
            supports_reasoning,
            context_window: Some(context_window),
            max_context_window: Some(context_window),
            auto_compact_token_limit: None,
            effective_context_window_percent: default_effective_context_window_percent(),
        }))
    }

    fn request_headers(&self) -> Result<HeaderMap, ResponsesApiError> {
        let mut request_headers = merge_request_headers(
            self.provider.headers(),
            HeaderMap::new(),
            &self.default_headers,
        );
        self.auth.add_headers(&mut request_headers)?;
        Ok(request_headers)
    }

    async fn send(&self, request: Request<Full<Bytes>>) -> Result<Bytes, ResponsesApiError> {
        let response = self
            .client
            .request(request)
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(transport_error)?
            .to_bytes();
        if !status.is_success() {
            let body = Some(String::from_utf8_lossy(&body).to_string());
            return Err(responses_http_error(status, headers, body));
        }
        Ok(body)
    }
}

fn ollama_supports_reasoning(capabilities: &[String]) -> bool {
    capabilities
        .iter()
        .any(|capability| capability == "thinking")
}

fn ollama_context_window(model_info: &HashMap<String, Value>) -> Option<u64> {
    model_info
        .iter()
        .filter(|(key, _)| key.ends_with(".context_length"))
        .filter_map(|(_, value)| value_as_u64(value))
        .max()
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_str()
        .and_then(|value| value.parse().ok())
        .or_else(|| {
            value
                .as_number()
                .and_then(|number| number.to_string().parse().ok())
        })
}

/// Codex-compatible `/models` client for model capability metadata.
///
/// Requests include the same `originator`/`User-Agent` defaults used for the
/// Responses WebSocket path and append `client_version` with
/// [`CODEX_CLI_CLIENT_VERSION`], matching Codex's model refresh mechanism.
#[derive(Debug, Clone)]
pub struct ResponsesModelsClient {
    provider: ApiProvider,
    auth: Auth,
    default_headers: HeaderMap,
    client: ApiHttpClient,
}

impl ResponsesModelsClient {
    /// Create a model metadata client using provider, auth, and Codex defaults.
    pub fn new(provider: ApiProvider, auth: Auth, default_headers: HeaderMap) -> Self {
        Self {
            provider,
            auth,
            default_headers,
            client: api_http_client(),
        }
    }

    /// Query model metadata from `/models?client_version=...`.
    pub async fn list_models(&self) -> Result<ResponsesModelsList, ResponsesApiError> {
        match self.list_models_once().await {
            Err(error)
                if error.is_unauthorized() && self.auth.refresh_after_unauthorized().await? =>
            {
                self.list_models_once().await
            }
            result => result,
        }
    }

    async fn list_models_once(&self) -> Result<ResponsesModelsList, ResponsesApiError> {
        let url = models_url(&self.provider);
        let mut request_headers = merge_request_headers(
            self.provider.headers(),
            HeaderMap::new(),
            &self.default_headers,
        );
        self.auth.add_headers(&mut request_headers)?;

        let mut request = Request::get(url.as_str())
            .body(Full::new(Bytes::new()))
            .map_err(|err| protocol_source_error("failed to build request", err))?;
        request.headers_mut().extend(request_headers);

        let response = self
            .client
            .request(request)
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let headers = response.headers().clone();
        let etag = headers
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(transport_error)?
            .to_bytes();

        if !status.is_success() {
            let body = Some(String::from_utf8_lossy(&body).to_string());
            return Err(responses_http_error(status, headers, body));
        }

        let response = sonic_rs::from_slice::<ResponsesModelsResponse>(&body).map_err(|err| {
            protocol_source_error(
                format!(
                    "failed to decode models response: {err}; body: {}",
                    String::from_utf8_lossy(&body)
                ),
                err,
            )
        })?;
        Ok(ResponsesModelsList {
            models: response.models,
            etag,
        })
    }

    /// Query `/models` and return metadata for one model slug.
    pub async fn model_info(
        &self,
        slug: &str,
    ) -> Result<Option<ResponsesModelInfo>, ResponsesApiError> {
        Ok(self.list_models().await?.model(slug).cloned())
    }
}

/// Complete request sent through the Responses WebSocket pool.
#[derive(Debug, Clone)]
pub struct ResponsesStreamRequest {
    /// Codex-compatible headers that identify the logical turn.
    pub headers: CodexHeaders,
    /// JSON `response.create` body sent as one WebSocket text frame.
    pub body: Value,
}

/// Event emitted while a Responses WebSocket request streams.
#[derive(Debug, Clone)]
pub enum ResponsesStreamEvent {
    /// Server handshake metadata reports whether reasoning is included.
    ServerReasoningIncluded(bool),
    /// Server handshake metadata carries the current `/models` `ETag`.
    ModelsEtag(String),
    /// Server handshake metadata reports the selected model slug.
    ServerModel(String),
    /// Raw JSON frame emitted by the Responses stream.
    Frame(Value),
    /// Terminal stream marker observed after a response completion frame.
    Completed {
        /// Response id returned by the terminal frame when present.
        response_id: Option<String>,
    },
}

/// Error returned by Responses HTTP, WebSocket, auth, and protocol operations.
#[derive(Debug, Error)]
pub enum ResponsesApiError {
    /// A caller-supplied HTTP header value is invalid.
    #[error("invalid header value for `{name}`")]
    Header {
        /// Header name that received an invalid value.
        name: &'static str,
    },
    /// The configured API provider URL is invalid.
    #[error("invalid API provider URL")]
    Url {
        /// URL parser error.
        #[source]
        source: url::ParseError,
    },
    /// The configured API provider URL has an unsupported scheme.
    #[error("unsupported API provider URL scheme {scheme}")]
    UnsupportedProviderScheme {
        /// Unsupported scheme value.
        scheme: String,
    },
    /// The configured API provider base URL contains a forbidden component.
    #[error("API provider base URL must not include {component}")]
    InvalidProviderBaseUrl {
        /// Forbidden URL component.
        component: &'static str,
    },
    /// Authentication failed or credential refresh failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// Backend returned a non-success HTTP response.
    #[error("HTTP error {status}{}", body.as_ref().map(|b| format!(": {b}")).unwrap_or_default())]
    Http {
        /// HTTP status code returned by the backend.
        status: StatusCode,
        /// Response headers returned by the backend.
        headers: HeaderMap,
        /// Response body when the backend supplied one.
        body: Option<String>,
    },
    /// Backend returned HTTP 403 Forbidden.
    #[error("HTTP error 403 Forbidden")]
    Forbidden {
        /// Response headers returned by the backend.
        headers: HeaderMap,
        /// Response body when the backend supplied one.
        body: Option<String>,
        /// Cloudflare Ray identifier when the 403 came from Cloudflare.
        cf_ray: Option<String>,
    },
    /// HTTP transport failure before a protocol response was available.
    #[error("HTTP transport error")]
    Transport {
        /// Source transport error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// WebSocket transport failure.
    #[error("websocket error: {message}")]
    WebSocket {
        /// Human-readable WebSocket failure.
        message: String,
        /// Source transport error when available.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },
    /// WebSocket operation exceeded its configured timeout.
    #[error("websocket timeout during {0}")]
    Timeout(&'static str),
    /// Responses protocol payload is malformed or unexpected.
    #[error("websocket protocol error: {message}")]
    Protocol {
        /// Human-readable protocol failure.
        message: String,
        /// Source codec/protocol error when available.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },
    /// Stream transport ended after at least one response frame without a terminal frame.
    #[error("response stream interrupted")]
    StreamInterrupted,
    /// Local new-connection budget is exhausted for the current minute.
    #[error("new Responses WebSocket connection rate limit reached: {limit}/minute")]
    ConnectionRateLimited {
        /// Configured maximum number of new connections per minute.
        limit: usize,
    },
    /// WebSocket operation may be retried on a fresh connection.
    #[error("retryable websocket error: {message}")]
    Retryable {
        /// Human-readable error message for the retryable failure.
        message: String,
        /// Server-requested delay before retrying, when provided.
        delay: Option<Duration>,
    },
}

impl ResponsesApiError {
    /// Return a human-readable error description including HTTP bodies when available.
    pub fn describe(&self) -> String {
        match self {
            Self::Http {
                status,
                body: Some(body),
                ..
            } if !body.is_empty() => format!("HTTP error {status}: {body}"),
            Self::Forbidden {
                body: Some(body),
                cf_ray,
                ..
            } if !body.is_empty() => match cf_ray {
                Some(cf_ray) => format!("HTTP error 403 Forbidden: {body}; cf ray: {cf_ray}"),
                None => format!("HTTP error 403 Forbidden: {body}"),
            },
            _ => self.to_string(),
        }
    }

    /// Returns true if the error is an HTTP 401 Unauthorized error.
    pub fn is_unauthorized(&self) -> bool {
        matches!(
            self,
            Self::Http {
                status: StatusCode::UNAUTHORIZED,
                ..
            }
        )
    }

    /// Returns true if the error requires a reconnection.
    pub fn requires_reconnect(&self) -> bool {
        matches!(
            self,
            Self::Retryable { .. }
                | Self::Forbidden {
                    cf_ray: Some(_),
                    ..
                }
        )
    }

    /// Returns true if the request can be retried before any response frame is received.
    pub fn can_retry_before_response_frame(&self) -> bool {
        matches!(
            self,
            Self::WebSocket { .. } | Self::Timeout(_) | Self::StreamInterrupted
        )
    }

    /// Convert the error into a retryable WebSocket error.
    pub fn into_retryable_websocket_error(self) -> Self {
        if matches!(self, Self::Retryable { .. }) {
            return self;
        }
        let message = self.describe();
        Self::Retryable {
            message,
            delay: None,
        }
    }

    /// Returns true if the WebSocket connection can be kept open after this error.
    pub fn can_keep_websocket_open_after_stream_error(&self) -> bool {
        matches!(self, Self::Http { status, .. } if *status != StatusCode::UNAUTHORIZED)
    }
}

/// Merges provider-specific, extra request, and default headers.
pub fn merge_request_headers(
    provider_headers: &HeaderMap,
    extra_headers: HeaderMap,
    default_headers: &HeaderMap,
) -> HeaderMap {
    let mut headers = provider_headers.clone();
    headers.extend(extra_headers);
    for (name, value) in default_headers {
        if let http::header::Entry::Vacant(entry) = headers.entry(name) {
            entry.insert(value.clone());
        }
    }
    headers
}

/// Build Codex identity headers from a caller-provided user agent.
///
/// The real Codex CLI sends `originator` and `User-Agent`; this helper does not
/// add a synthetic version header.
pub fn codex_default_headers(user_agent: &str) -> Result<HeaderMap, ResponsesApiError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static(ORIGINATOR),
        HeaderValue::from_static(DEFAULT_CODEX_ORIGINATOR),
    );
    let user_agent = HeaderValue::from_str(user_agent)
        .map_err(|_| ResponsesApiError::Header { name: "user-agent" })?;
    headers.insert(USER_AGENT, user_agent);
    Ok(headers)
}

/// Build default Codex identity headers for this lean harness.
///
/// The `User-Agent` mirrors the current Codex CLI client version instead of
/// this harness crate's package version, while preserving stock Codex's
/// `originator/version (os version; arch) terminal` shape. If a `/models`
/// refresh endpoint is added, its `client_version` query parameter should use
/// the same [`CODEX_CLI_CLIENT_VERSION`] value.
pub fn lean_codex_default_headers() -> HeaderMap {
    codex_default_headers(CODEX_CLI_USER_AGENT).expect("lean Codex user-agent is a valid header")
}

fn insert_header(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), ResponsesApiError> {
    let value = HeaderValue::from_str(value).map_err(|_| ResponsesApiError::Header { name })?;
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

/// Map a WebSocket library error into a Responses API error.
pub fn map_ws_error(err: WsError, url: &Url) -> ResponsesApiError {
    match err {
        WsError::Http(response) => {
            let status = response.status();
            let headers = response.headers().clone();
            let body = response
                .body()
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok());
            responses_http_error(status, headers, body)
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => websocket_error("websocket closed"),
        WsError::Io(err) => websocket_source_error("websocket I/O error", err),
        other => websocket_source_error(format!("websocket error for url {url}"), other),
    }
}

#[derive(Debug, Deserialize)]
struct WrappedWebsocketError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WrappedWebsocketErrorEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(alias = "status_code")]
    status: Option<u16>,
    #[serde(default)]
    error: Option<WrappedWebsocketError>,
    #[serde(default)]
    headers: Option<Object>,
}

/// Decode and map a wrapped Cloudflare/WebSocket error payload.
pub fn map_wrapped_websocket_error(payload: &str) -> Option<ResponsesApiError> {
    let event = sonic_rs::from_str::<WrappedWebsocketErrorEvent>(payload).ok()?;
    if event.kind != "error" {
        return None;
    }
    if let Some(error) = event.error.as_ref()
        && error.code.as_deref() == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE)
    {
        return Some(ResponsesApiError::Retryable {
            message: error
                .message
                .clone()
                .unwrap_or_else(|| WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE.to_string()),
            delay: None,
        });
    }
    let status = StatusCode::from_u16(event.status?).ok()?;
    if status.is_success() {
        return None;
    }
    let headers = event
        .headers
        .map(json_headers_to_http_headers)
        .unwrap_or_default();
    Some(responses_http_error(
        status,
        headers,
        Some(payload.to_string()),
    ))
}

fn json_headers_to_http_headers(headers: Object) -> HeaderMap {
    let mut mapped = HeaderMap::new();
    for (name, value) in headers.iter() {
        let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Some(header_value) = json_header_value(value.clone()) else {
            continue;
        };
        mapped.insert(header_name, header_value);
    }
    mapped
}

fn json_header_value(value: Value) -> Option<HeaderValue> {
    let value = value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_number().map(|value| value.to_string()))
        .or_else(|| value.as_bool().map(|value| value.to_string()))?;
    HeaderValue::from_str(&value).ok()
}

fn responses_http_error(
    status: StatusCode,
    headers: HeaderMap,
    body: Option<String>,
) -> ResponsesApiError {
    if status == StatusCode::FORBIDDEN {
        let cf_ray = cf_ray(&headers, body.as_deref());
        return ResponsesApiError::Forbidden {
            headers,
            body,
            cf_ray,
        };
    }
    ResponsesApiError::Http {
        status,
        headers,
        body,
    }
}

fn cf_ray(headers: &HeaderMap, body: Option<&str>) -> Option<String> {
    headers
        .get("cf-ray")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
        .or_else(|| body.and_then(cf_ray_from_body))
}

fn cf_ray_from_body(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    lower.contains("cf ray").then(|| "cf ray".to_string())
}

/// Stamps turn metadata indicating prewarm to false.
pub fn stamp_prewarm_generate_false(body: &mut Value) -> Result<(), ResponsesApiError> {
    let root = body
        .as_object_mut()
        .ok_or_else(|| protocol_error("Responses WebSocket frame must be a JSON object"))?;
    root.insert(&"generate", false);
    Ok(())
}

fn api_http_client() -> ApiHttpClient {
    let https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    HyperClient::builder(TokioExecutor::new()).build(https)
}

fn refresh_http_client() -> RefreshHttpClient {
    api_http_client()
}

fn models_url(provider: &ApiProvider) -> Url {
    let mut url = provider.endpoint_url(ApiEndpoint::Models);
    url.query_pairs_mut()
        .append_pair("client_version", CODEX_CLI_CLIENT_VERSION);
    url
}

fn validate_provider_base_url(base_url: &Url) -> Result<(), ResponsesApiError> {
    if websocket_scheme(base_url.scheme()).is_none() {
        return Err(ResponsesApiError::UnsupportedProviderScheme {
            scheme: base_url.scheme().to_string(),
        });
    }
    if base_url.query().is_some() {
        return Err(ResponsesApiError::InvalidProviderBaseUrl { component: "query" });
    }
    if base_url.fragment().is_some() {
        return Err(ResponsesApiError::InvalidProviderBaseUrl {
            component: "fragment",
        });
    }
    Ok(())
}

fn normalize_provider_base_url(base_url: &mut Url) {
    if !base_url.path().ends_with('/') {
        base_url
            .path_segments_mut()
            .expect("validated provider URL is path-segment based")
            .push("");
    }
}

fn websocket_scheme(scheme: &str) -> Option<&'static str> {
    match scheme {
        "http" => Some("ws"),
        "https" => Some("wss"),
        "ws" => Some("ws"),
        "wss" => Some("wss"),
        _ => None,
    }
}

/// Return the ChatGPT refresh-token endpoint selected for this process.
pub fn refresh_token_endpoint() -> String {
    std::env::var(CODEX_REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR)
        .unwrap_or_else(|_| CHATGPT_REFRESH_TOKEN_URL.to_string())
}

fn refresh_error_message(status: StatusCode, body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    let code = extract_refresh_error_code(&body);
    if status == StatusCode::UNAUTHORIZED {
        return match code.as_deref() {
            Some("refresh_token_expired") => "refresh token expired".to_string(),
            Some("refresh_token_reused") => "refresh token already used".to_string(),
            Some("refresh_token_invalidated") => "refresh token revoked".to_string(),
            Some(code) => format!("refresh token rejected with code `{code}`"),
            None => "refresh token rejected".to_string(),
        };
    }
    match code {
        Some(code) => format!("refresh token request failed with HTTP {status}: {code}"),
        None => format!("refresh token request failed with HTTP {status}: {body}"),
    }
}

fn extract_refresh_error_code(body: &str) -> Option<String> {
    if body.trim().is_empty() {
        return None;
    }
    let value = sonic_rs::from_str::<Value>(body).ok()?;
    let map = value.as_object()?;
    if let Some(error) = map.get(&"error") {
        if let Some(error) = error.as_object() {
            if let Some(code) = error.get(&"code").and_then(JsonValueTrait::as_str) {
                return Some(code.to_string());
            }
        } else if let Some(code) = error.as_str() {
            return Some(code.to_string());
        }
    }
    map.get(&"code")
        .and_then(JsonValueTrait::as_str)
        .map(ToOwned::to_owned)
}

fn event_type(frame: &Value) -> Option<&str> {
    frame.as_object()?.get(&"type")?.as_str()
}

/// Returns true if the WebSocket frame represents a terminal event.
pub fn is_response_terminal_event(frame: &Value) -> bool {
    matches!(
        event_type(frame),
        Some("response.completed" | "response.done")
    )
}

/// Extracts the completed response ID from a WebSocket frame, if available.
pub fn response_completed_id(frame: &Value) -> Option<String> {
    frame
        .as_object()?
        .get(&"response")?
        .as_object()?
        .get(&"id")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn now_unix_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        num::NonZeroUsize,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
    };

    use futures_util::{SinkExt, StreamExt};
    use sonic_rs::json;
    use tokio::{net::TcpListener as TokioTcpListener, sync::oneshot};
    use tokio_tungstenite::{
        accept_hdr_async,
        tungstenite::{
            Message,
            handshake::server::{Request as ServerRequest, Response as ServerResponse},
        },
    };

    use super::*;

    fn http_header_value(head: &str, name: &str) -> Option<String> {
        head.lines().find_map(|line| {
            let (header_name, value) = line.split_once(':')?;
            header_name
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }

    #[test]
    fn response_done_is_terminal_websocket_event() {
        assert!(is_response_terminal_event(&json!({
            "type": "response.done",
            "response": { "id": "resp_done" }
        })));
        assert!(is_response_terminal_event(&json!({
            "type": "response.completed",
            "response": { "id": "resp_completed" }
        })));
        assert!(!is_response_terminal_event(&json!({
            "type": "response.output_text.done"
        })));
    }

    #[test]
    fn websocket_endpoint_url_converts_https_responses_url() {
        let provider = ApiProvider::new("https://api.example.invalid/codex").unwrap();
        let url = provider.websocket_endpoint_url(ApiEndpoint::Responses);
        assert_eq!(url.as_str(), "wss://api.example.invalid/codex/responses");
    }

    #[test]
    fn endpoint_url_does_not_infer_version_prefix() {
        let provider = ApiProvider::new("https://api.example.invalid").unwrap();

        assert_eq!(
            provider.endpoint_url(ApiEndpoint::Models).as_str(),
            "https://api.example.invalid/models"
        );
        assert_eq!(
            provider
                .websocket_endpoint_url(ApiEndpoint::Responses)
                .as_str(),
            "wss://api.example.invalid/responses"
        );
    }

    #[test]
    fn lean_codex_default_headers_match_static_codex_identity_shape() {
        let headers = lean_codex_default_headers();

        assert_eq!(
            headers.get(ORIGINATOR),
            Some(&HeaderValue::from_static(DEFAULT_CODEX_ORIGINATOR))
        );
        assert_eq!(
            headers.get(USER_AGENT),
            Some(&HeaderValue::from_static(CODEX_CLI_USER_AGENT))
        );
        assert_eq!(
            CODEX_CLI_USER_AGENT,
            format!(
                "{DEFAULT_CODEX_ORIGINATOR}/{CODEX_CLI_CLIENT_VERSION} (Linux 6.8.0; x86_64) unknown"
            )
        );
        assert!(CODEX_CLI_USER_AGENT.starts_with(&format!(
            "{DEFAULT_CODEX_ORIGINATOR}/{CODEX_CLI_CLIENT_VERSION} "
        )));
    }

    #[test]
    fn codex_default_headers_do_not_add_synthetic_version_header() {
        let headers = lean_codex_default_headers();

        assert!(!headers.contains_key("version"));
        assert!(!headers.contains_key("client-version"));
        assert!(!headers.contains_key("x-client-version"));
        assert!(!headers.contains_key("x-codex-client-version"));
    }
}

#[cfg(any())]
mod obsolete_tests {
    use super::*;

    #[tokio::test]
    async fn models_client_queries_client_version_and_parallel_capability() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 512];
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8(request).unwrap();
            request_tx.send(head).unwrap();

            let response_body =
                br#"{"models":[{"slug":"gpt-test","supports_parallel_tool_calls":true}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"models-1\"\r\nContent-Length: {}\r\n\r\n",
                response_body.len()
            )
            .unwrap();
            stream.write_all(response_body).unwrap();
        });

        let client = ResponsesModelsClient::new(
            ApiProvider::new(format!("http://{addr}/codex")).unwrap(),
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
        );

        let list = client.list_models().await.unwrap();

        assert_eq!(list.etag.as_deref(), Some("\"models-1\""));
        assert_eq!(
            list.model("gpt-test"),
            Some(&ResponsesModelInfo {
                slug: "gpt-test".to_string(),
                supports_parallel_tool_calls: true,
                supports_reasoning: false,
                context_window: None,
                max_context_window: None,
                auto_compact_token_limit: None,
                effective_context_window_percent: 95,
            })
        );

        let head = request_rx.recv().unwrap();
        assert!(head.starts_with(&format!(
            "GET /codex/models?client_version={CODEX_CLI_CLIENT_VERSION} HTTP/1.1"
        )));
        assert_eq!(
            http_header_value(&head, ORIGINATOR),
            Some(DEFAULT_CODEX_ORIGINATOR.to_string())
        );
        assert_eq!(
            http_header_value(&head, "user-agent"),
            Some(CODEX_CLI_USER_AGENT.to_string())
        );
        assert_eq!(
            http_header_value(&head, "authorization"),
            Some("Bearer api-token".to_string())
        );
        server.join().unwrap();
    }
    #[tokio::test]
    async fn ollama_model_info_client_reads_context_window_from_native_show() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
                if let Some(body_start) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let head = String::from_utf8(request[..body_start].to_vec()).unwrap();
                    let content_length = http_header_value(&head, "content-length")
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    let body_start = body_start + 4;
                    while request.len() < body_start + content_length {
                        let read = stream.read(&mut buffer).unwrap();
                        assert!(read > 0);
                        request.extend_from_slice(&buffer[..read]);
                    }
                    let body = request[body_start..body_start + content_length].to_vec();
                    request_tx.send((head, body)).unwrap();
                    break;
                }
            }

            let body = br#"{"model_info":{"general.architecture":"glm","glm.context_length":131072,"tokenizer.ggml.model":"gpt2"}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let client = OllamaModelInfoClient::new(
            ApiProvider::new(format!("http://{addr}/v1")).unwrap(),
            Auth::ApiKey("ollama-token".to_string()),
            HeaderMap::new(),
        );
        let info = client.model_info("glm-5.2").await.unwrap().unwrap();

        assert_eq!(
            info,
            ResponsesModelInfo {
                slug: "glm-5.2".to_string(),
                supports_parallel_tool_calls: false,
                supports_reasoning: false,
                context_window: Some(131_072),
                max_context_window: Some(131_072),
                auto_compact_token_limit: None,
                effective_context_window_percent: 95,
            }
        );
        let (request_head, request_body) = request_rx.recv().unwrap();
        assert!(request_head.starts_with("POST /api/show HTTP/1.1"));
        assert_eq!(
            http_header_value(&request_head, "authorization"),
            Some("Bearer ollama-token".to_string())
        );
        let body = sonic_rs::from_slice::<Value>(&request_body).unwrap();
        assert_eq!(
            body.as_object()
                .unwrap()
                .get(&"model")
                .and_then(JsonValueTrait::as_str),
            Some("glm-5.2")
        );
        server.join().unwrap();
    }

    #[derive(Debug)]
    struct RotatingTestAuth {
        refreshed: AtomicBool,
    }

    impl ChatGptAuthSession for RotatingTestAuth {
        fn access_token(&self) -> Result<String, AuthError> {
            let token = if self.refreshed.load(Ordering::SeqCst) {
                "access-new"
            } else {
                "access-old"
            };
            Ok(token.to_string())
        }

        fn account_id(&self) -> Result<Option<String>, AuthError> {
            Ok(None)
        }

        fn refresh_after_unauthorized(&self) -> AuthRefreshFuture<'_> {
            Box::pin(async move {
                self.refreshed.store(true, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn models_client_refreshes_chatgpt_auth_after_unauthorized() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for response_index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 512];
                    let read = stream.read(&mut buffer).unwrap();
                    assert_ne!(read, 0);
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8(request).unwrap();
                request_tx.send(head).unwrap();

                if response_index == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\n\r\nunauthorized",
                        )
                        .unwrap();
                } else {
                    let response_body =
                        br#"{"models":[{"slug":"gpt-test","supports_parallel_tool_calls":true}]}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        response_body.len()
                    )
                    .unwrap();
                    stream.write_all(response_body).unwrap();
                }
            }
        });

        let auth = Auth::ChatGpt(Arc::new(RotatingTestAuth {
            refreshed: AtomicBool::new(false),
        }));
        let client = ResponsesModelsClient::new(
            ApiProvider::new(format!("http://{addr}/codex")).unwrap(),
            auth,
            lean_codex_default_headers(),
        );

        let list = client.list_models().await.unwrap();

        assert!(list.model("gpt-test").is_some());
        let first_head = request_rx.recv().unwrap();
        assert_eq!(
            http_header_value(&first_head, "authorization"),
            Some("Bearer access-old".to_string())
        );
        let second_head = request_rx.recv().unwrap();
        assert_eq!(
            http_header_value(&second_head, "authorization"),
            Some("Bearer access-new".to_string())
        );
        server.join().unwrap();
    }

    #[test]
    fn model_info_accepts_camel_case_capability_metadata() {
        let body = br#"{"models":[{"slug":"gpt-test","supportsParallelToolCalls":true,"contextWindow":300000,"maxContextWindow":400000,"autoCompactTokenLimit":285000,"effectiveContextWindowPercent":100}]}"#;

        let response = sonic_rs::from_slice::<ResponsesModelsResponse>(body).unwrap();

        assert_eq!(
            response.models,
            vec![ResponsesModelInfo {
                slug: "gpt-test".to_string(),
                supports_parallel_tool_calls: true,
                supports_reasoning: false,
                context_window: Some(300_000),
                max_context_window: Some(400_000),
                auto_compact_token_limit: Some(285_000),
                effective_context_window_percent: 100,
            }]
        );
    }

    #[test]
    fn model_info_accepts_openai_compatible_data_models() {
        let body = br#"{"object":"list","data":[{"id":"gpt-oss:20b","object":"model","created":1754352000,"owned_by":"ollama"}]}"#;
        let response = sonic_rs::from_slice::<ResponsesModelsResponse>(body).unwrap();

        assert_eq!(
            response.models,
            vec![ResponsesModelInfo {
                slug: "gpt-oss:20b".to_string(),
                supports_parallel_tool_calls: false,
                supports_reasoning: false,
                context_window: None,
                max_context_window: None,
                auto_compact_token_limit: None,
                effective_context_window_percent: 95,
            }]
        );
    }

    #[test]
    fn model_info_derives_effective_context_and_auto_compact_limit() {
        let model_info = ResponsesModelInfo {
            slug: "gpt-test".to_string(),
            supports_parallel_tool_calls: true,
            supports_reasoning: false,
            context_window: Some(272_000),
            max_context_window: Some(1_000_000),
            auto_compact_token_limit: None,
            effective_context_window_percent: 95,
        };

        assert_eq!(model_info.auto_compact_token_limit(), Some(245_480));
    }

    #[test]
    fn codex_headers_include_responses_websocket_contract() {
        let turn_state = ResponsesTurnState::new();
        turn_state.capture("turn-state".to_string());
        let mut headers = CodexHeaders::for_thread("session", "thread", "thread:0");
        headers.installation_id = Some("install".to_string());
        headers.parent_thread_id = Some("parent".to_string());
        headers.subagent = Some("collab_spawn".to_string());
        headers.beta_features = Some("feature".to_string());
        headers.turn_state = Some(turn_state);
        headers.turn_metadata = Some("{\"turn_id\":\"turn\"}".to_string());
        headers.include_timing_metrics = true;

        let map = headers.to_header_map().unwrap();

        assert_eq!(map[OPENAI_BETA_HEADER], OPENAI_BETA_RESPONSES_WEBSOCKETS_V2);
        assert_eq!(map[X_CLIENT_REQUEST_ID], "thread");
        assert_eq!(map[SESSION_ID], "session");
        assert_eq!(map[THREAD_ID], "thread");
        assert_eq!(map[X_CODEX_WINDOW_ID], "thread:0");
        assert_eq!(map[X_CODEX_PARENT_THREAD_ID], "parent");
        assert_eq!(map[X_OPENAI_SUBAGENT], "collab_spawn");
        assert_eq!(map[X_CODEX_BETA_FEATURES], "feature");
        assert_eq!(map[X_CODEX_TURN_STATE], "turn-state");
        assert_eq!(map[X_RESPONSESAPI_INCLUDE_TIMING_METRICS], "true");
        assert!(!map.contains_key(X_CODEX_INSTALLATION_ID));
    }

    #[test]
    fn stamps_codex_client_metadata_with_sonic_value() {
        let mut headers = CodexHeaders::for_thread("session", "thread", "thread:0");
        headers.installation_id = Some("install".to_string());
        let mut body = json!({
            "type": "response.create",
            "model": "gpt-test"
        });

        headers.stamp_client_metadata(&mut body).unwrap();
        let metadata = body
            .as_object()
            .unwrap()
            .get(&"client_metadata")
            .unwrap()
            .as_object()
            .unwrap();

        assert_eq!(
            metadata.get(&X_CODEX_INSTALLATION_ID).unwrap().as_str(),
            Some("install")
        );
        assert_eq!(
            metadata
                .get(&X_CODEX_WS_STREAM_REQUEST_START_MS)
                .unwrap()
                .as_str()
                .is_some(),
            true
        );
    }

    #[test]
    fn prewarm_stamps_generate_false() {
        let mut body = json!({
            "type": "response.create",
            "model": "gpt-test",
            "generate": true
        });

        stamp_prewarm_generate_false(&mut body).unwrap();

        assert_eq!(
            body.as_object()
                .unwrap()
                .get(&"generate")
                .unwrap()
                .as_bool(),
            Some(false)
        );
    }

    #[test]
    fn connection_context_does_not_partition_by_request_headers() {
        let first_turn_state = ResponsesTurnState::new();
        first_turn_state.capture("first".to_string());
        let mut first_headers = CodexHeaders::for_thread("session-a", "thread-a", "thread-a:0");
        first_headers.installation_id = Some("first-install".to_string());
        first_headers.parent_thread_id = Some("parent-a".to_string());
        first_headers.subagent = Some("collab_spawn".to_string());
        first_headers.beta_features = Some("feature-a".to_string());
        first_headers.turn_state = Some(first_turn_state);
        first_headers.turn_metadata = Some("{\"turn_id\":\"first\"}".to_string());
        first_headers.include_timing_metrics = true;

        let second_turn_state = ResponsesTurnState::new();
        second_turn_state.capture("second".to_string());
        let mut second_headers = CodexHeaders::for_thread("session-b", "thread-b", "thread-b:1");
        second_headers.installation_id = Some("second-install".to_string());
        second_headers.parent_thread_id = Some("parent-b".to_string());
        second_headers.subagent = Some("codex_cli_rs".to_string());
        second_headers.beta_features = Some("feature-b".to_string());
        second_headers.turn_state = Some(second_turn_state);
        second_headers.turn_metadata = Some("{\"turn_id\":\"second\"}".to_string());
        second_headers.include_timing_metrics = false;

        assert_eq!(
            ConnectionContext::from_headers(&first_headers),
            ConnectionContext::from_headers(&second_headers)
        );
    }

    #[test]
    fn merged_headers_match_codex_precedence_with_auth_last() {
        let mut provider_headers = HeaderMap::new();
        provider_headers.insert(
            "originator",
            HeaderValue::from_static("provider-originator"),
        );
        provider_headers.insert("authorization", HeaderValue::from_static("Bearer provider"));
        provider_headers.insert("x-priority", HeaderValue::from_static("provider"));

        let mut extra_headers = HeaderMap::new();
        extra_headers.insert("originator", HeaderValue::from_static("extra-originator"));
        extra_headers.insert("x-priority", HeaderValue::from_static("extra"));
        extra_headers.insert("x-extra-only", HeaderValue::from_static("extra-only"));

        let mut default_headers = HeaderMap::new();
        default_headers.insert("originator", HeaderValue::from_static("default-originator"));
        default_headers.insert("x-priority", HeaderValue::from_static("default"));
        default_headers.insert("x-default-only", HeaderValue::from_static("default-only"));

        let mut merged = merge_request_headers(&provider_headers, extra_headers, &default_headers);
        Auth::ApiKey("auth-token".to_string())
            .add_headers(&mut merged)
            .unwrap();

        assert_eq!(
            merged.get("originator"),
            Some(&HeaderValue::from_static("extra-originator"))
        );
        assert_eq!(
            merged.get("x-priority"),
            Some(&HeaderValue::from_static("extra"))
        );
        assert_eq!(
            merged.get("x-extra-only"),
            Some(&HeaderValue::from_static("extra-only"))
        );
        assert_eq!(
            merged.get("x-default-only"),
            Some(&HeaderValue::from_static("default-only"))
        );
        assert_eq!(
            merged.get(AUTHORIZATION),
            Some(&HeaderValue::from_static("Bearer auth-token"))
        );
    }

    #[tokio::test]
    async fn pool_prewarm_then_stream_reuses_socket_and_sends_codex_wire_contract() {
        let listener = TokioTcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (handshake_tx, handshake_rx) = oneshot::channel();
        let (messages_tx, messages_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let callback = |request: &ServerRequest, mut response: ServerResponse| {
                assert_eq!(request.uri().path(), "/codex/responses");
                handshake_tx.send(request.headers().clone()).unwrap();
                response
                    .headers_mut()
                    .insert(X_CODEX_TURN_STATE, HeaderValue::from_static("server-turn"));
                Ok(response)
            };
            let mut ws = accept_hdr_async(stream, callback).await.unwrap();
            let mut messages = Vec::new();
            for response_id in ["prewarm", "stream"] {
                let text = loop {
                    match ws.next().await.unwrap().unwrap() {
                        Message::Text(text) => break text.to_string(),
                        Message::Ping(payload) => ws.send(Message::Pong(payload)).await.unwrap(),
                        message => panic!("unexpected websocket test message: {message:?}"),
                    }
                };
                messages.push(text);
                ws.send(Message::Text(
                    format!(
                        r#"{{"type":"response.completed","response":{{"id":"{response_id}"}}}}"#
                    )
                    .into(),
                ))
                .await
                .unwrap();
            }
            messages_tx.send(messages).unwrap();
        });

        let provider = ApiProvider::new(format!("ws://{addr}/codex")).unwrap();
        let pool = ResponsesWsPool::new(
            provider,
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
            WsPoolConfig {
                min_new_connection_interval: Duration::ZERO,
                connect_timeout: Duration::from_secs(1),
                ..Default::default()
            },
        );
        let turn_state = ResponsesTurnState::new();
        let mut headers = CodexHeaders::for_thread("session", "thread", "thread:0");
        headers.installation_id = Some("install".to_string());
        headers.parent_thread_id = Some("parent".to_string());
        headers.subagent = Some("collab_spawn".to_string());
        headers.beta_features = Some("feature".to_string());
        headers.turn_state = Some(turn_state.clone());
        headers.turn_metadata = Some("{\"turn_id\":\"turn\"}".to_string());
        headers.include_timing_metrics = true;

        pool.prewarm(ResponsesStreamRequest {
            headers: headers.clone(),
            body: json!({
                "type": "response.create",
                "model": "gpt-test",
                "stream": true
            }),
        })
        .await
        .unwrap();
        assert_eq!(turn_state.get(), Some("server-turn"));

        let mut stream_headers = CodexHeaders::for_thread("session-2", "thread-2", "thread-2:1");
        stream_headers.installation_id = Some("install-2".to_string());
        stream_headers.parent_thread_id = Some("parent-2".to_string());
        stream_headers.subagent = Some("codex_cli_rs".to_string());
        stream_headers.beta_features = Some("feature-2".to_string());
        stream_headers.turn_metadata = Some("{\"turn_id\":\"turn-2\"}".to_string());

        pool.stream_request(
            ResponsesStreamRequest {
                headers: stream_headers,
                body: json!({
                    "type": "response.create",
                    "model": "gpt-test",
                    "stream": true
                }),
            },
            |_event| std::future::ready(()),
        )
        .await
        .unwrap();

        let handshake = handshake_rx.await.unwrap();
        assert_eq!(
            handshake[OPENAI_BETA_HEADER],
            OPENAI_BETA_RESPONSES_WEBSOCKETS_V2
        );
        assert_eq!(handshake[X_CLIENT_REQUEST_ID], "thread");
        assert_eq!(handshake[SESSION_ID], "session");
        assert_eq!(handshake[THREAD_ID], "thread");
        assert_eq!(handshake[X_CODEX_WINDOW_ID], "thread:0");
        assert_eq!(handshake[X_CODEX_PARENT_THREAD_ID], "parent");
        assert_eq!(handshake[X_OPENAI_SUBAGENT], "collab_spawn");
        assert_eq!(handshake[X_CODEX_BETA_FEATURES], "feature");
        assert_eq!(handshake[X_CODEX_TURN_METADATA], "{\"turn_id\":\"turn\"}");
        assert_eq!(handshake[X_RESPONSESAPI_INCLUDE_TIMING_METRICS], "true");
        assert_eq!(handshake[ORIGINATOR], DEFAULT_CODEX_ORIGINATOR);
        assert_eq!(handshake[USER_AGENT], CODEX_CLI_USER_AGENT);
        assert_eq!(handshake[AUTHORIZATION], "Bearer api-token");
        assert!(!handshake.contains_key(X_CODEX_INSTALLATION_ID));

        let messages = messages_rx.await.unwrap();
        assert_eq!(messages.len(), 2);
        let prewarm = sonic_rs::from_str::<Value>(&messages[0]).unwrap();
        let prewarm = prewarm.as_object().unwrap();
        assert_eq!(prewarm.get(&"generate").unwrap().as_bool(), Some(false));
        let prewarm_metadata = prewarm
            .get(&"client_metadata")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(
            prewarm_metadata
                .get(&X_CODEX_INSTALLATION_ID)
                .and_then(JsonValueTrait::as_str),
            Some("install")
        );

        let stream = sonic_rs::from_str::<Value>(&messages[1]).unwrap();
        let stream = stream.as_object().unwrap();
        assert!(stream.get(&"generate").is_none());
        let stream_metadata = stream.get(&"client_metadata").unwrap().as_object().unwrap();
        assert_eq!(
            stream_metadata
                .get(&X_CODEX_INSTALLATION_ID)
                .and_then(JsonValueTrait::as_str),
            Some("install-2")
        );
        assert_eq!(
            stream_metadata
                .get(&X_CODEX_WINDOW_ID)
                .and_then(JsonValueTrait::as_str),
            Some("thread-2:1")
        );
        assert_eq!(
            stream_metadata
                .get(&X_CODEX_PARENT_THREAD_ID)
                .and_then(JsonValueTrait::as_str),
            Some("parent-2")
        );
        assert_eq!(
            stream_metadata
                .get(&X_OPENAI_SUBAGENT)
                .and_then(JsonValueTrait::as_str),
            Some("codex_cli_rs")
        );
        assert_eq!(
            stream_metadata
                .get(&X_CODEX_TURN_METADATA)
                .and_then(JsonValueTrait::as_str),
            Some("{\"turn_id\":\"turn-2\"}")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn idle_websocket_responds_to_server_ping_before_reuse() {
        let listener = TokioTcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (pong_tx, pong_rx) = oneshot::channel();
        let (messages_tx, messages_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws =
                accept_hdr_async(stream, |_request: &ServerRequest, response| Ok(response))
                    .await
                    .unwrap();
            let mut messages = Vec::new();
            let first = ws.next().await.unwrap().unwrap();
            let Message::Text(first) = first else {
                panic!("expected first request text frame");
            };
            messages.push(first.to_string());
            ws.send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"first"}}"#.into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Ping(Bytes::from_static(b"idle-ping")))
                .await
                .unwrap();

            loop {
                let message = tokio::time::timeout(Duration::from_secs(1), ws.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                match message {
                    Message::Pong(payload) => {
                        assert_eq!(payload.as_ref(), b"idle-ping");
                        pong_tx.send(()).unwrap();
                        break;
                    }
                    Message::Ping(payload) => ws.send(Message::Pong(payload)).await.unwrap(),
                    message => panic!("expected idle pong, got {message:?}"),
                }
            }

            let second = ws.next().await.unwrap().unwrap();
            let Message::Text(second) = second else {
                panic!("expected second request text frame");
            };
            messages.push(second.to_string());
            ws.send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"second"}}"#.into(),
            ))
            .await
            .unwrap();
            messages_tx.send(messages).unwrap();
        });

        let pool = ResponsesWsPool::new(
            ApiProvider::new(format!("ws://{addr}/codex")).unwrap(),
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
            WsPoolConfig {
                min_new_connection_interval: Duration::ZERO,
                ready_connections_per_active_connection: NonZeroUsize::new(1)
                    .expect("1 is non-zero"),
                connect_timeout: Duration::from_secs(1),
                idle_maintenance_interval: Duration::from_millis(10),
                ..Default::default()
            },
        );
        let headers = CodexHeaders::for_thread("session", "thread", "thread:0");

        pool.stream_request(
            ResponsesStreamRequest {
                headers: headers.clone(),
                body: json!({"type": "response.create", "model": "gpt-test"}),
            },
            |_event| std::future::ready(()),
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), pong_rx)
            .await
            .unwrap()
            .unwrap();

        pool.stream_request(
            ResponsesStreamRequest {
                headers,
                body: json!({"type": "response.create", "model": "gpt-test"}),
            },
            |_event| std::future::ready(()),
        )
        .await
        .unwrap();

        let messages = messages_rx.await.unwrap();
        assert_eq!(messages.len(), 2);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn request_level_websocket_error_keeps_connection_reusable() {
        let listener = TokioTcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (messages_tx, messages_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws =
                accept_hdr_async(stream, |_request: &ServerRequest, response| Ok(response))
                    .await
                    .unwrap();
            let mut messages = Vec::new();
            let first = ws.next().await.unwrap().unwrap();
            let Message::Text(first) = first else {
                panic!("expected first request text frame");
            };
            messages.push(first.to_string());
            ws.send(Message::Text(
                r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limited"}}"#
                    .into(),
            ))
            .await
            .unwrap();

            let second = ws.next().await.unwrap().unwrap();
            let Message::Text(second) = second else {
                panic!("expected second request text frame");
            };
            messages.push(second.to_string());
            ws.send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"second"}}"#.into(),
            ))
            .await
            .unwrap();
            messages_tx.send(messages).unwrap();
        });

        let pool = ResponsesWsPool::new(
            ApiProvider::new(format!("ws://{addr}/codex")).unwrap(),
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
            WsPoolConfig {
                min_new_connection_interval: Duration::ZERO,
                connect_timeout: Duration::from_secs(1),
                ..Default::default()
            },
        );
        let headers = CodexHeaders::for_thread("session", "thread", "thread:0");

        let first_result = pool
            .stream_request(
                ResponsesStreamRequest {
                    headers: headers.clone(),
                    body: json!({"type": "response.create", "model": "gpt-test"}),
                },
                |_event| std::future::ready(()),
            )
            .await;
        let Err(ResponsesApiError::Http { status, .. }) = first_result else {
            panic!("expected request-level websocket HTTP error");
        };
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

        pool.stream_request(
            ResponsesStreamRequest {
                headers,
                body: json!({"type": "response.create", "model": "gpt-test"}),
            },
            |_event| std::future::ready(()),
        )
        .await
        .unwrap();

        let messages = messages_rx.await.unwrap();
        assert_eq!(messages.len(), 2);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn websocket_connection_limit_error_reconnects_and_retries() {
        let listener = TokioTcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (messages_tx, messages_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut messages = Vec::new();
            let (first_stream, _) = listener.accept().await.unwrap();
            let mut first_ws =
                accept_hdr_async(first_stream, |_request: &ServerRequest, response| {
                    Ok(response)
                })
                .await
                .unwrap();
            let first = first_ws.next().await.unwrap().unwrap();
            let Message::Text(first) = first else {
                panic!("expected first request text frame");
            };
            messages.push(first.to_string());
            first_ws
                .send(Message::Text(
                    format!(
                        r#"{{"type":"error","error":{{"code":"{WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE}","message":"limit"}}}}"#
                    )
                    .into(),
                ))
                .await
                .unwrap();

            let (second_stream, _) = listener.accept().await.unwrap();
            let mut second_ws =
                accept_hdr_async(second_stream, |_request: &ServerRequest, response| {
                    Ok(response)
                })
                .await
                .unwrap();
            let second = second_ws.next().await.unwrap().unwrap();
            let Message::Text(second) = second else {
                panic!("expected retry request text frame");
            };
            messages.push(second.to_string());
            second_ws
                .send(Message::Text(
                    r#"{"type":"response.completed","response":{"id":"retry"}}"#.into(),
                ))
                .await
                .unwrap();
            messages_tx.send(messages).unwrap();
        });

        let pool = ResponsesWsPool::new(
            ApiProvider::new(format!("ws://{addr}/codex")).unwrap(),
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
            WsPoolConfig {
                min_new_connection_interval: Duration::ZERO,
                connect_timeout: Duration::from_secs(1),
                keep_connections_open: false,
                ..Default::default()
            },
        );

        pool.stream_request(
            ResponsesStreamRequest {
                headers: CodexHeaders::for_thread("session", "thread", "thread:0"),
                body: json!({"type": "response.create", "model": "gpt-test"}),
            },
            |_event| std::future::ready(()),
        )
        .await
        .unwrap();

        let messages = messages_rx.await.unwrap();
        assert_eq!(messages.len(), 2);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cloudflare_forbidden_websocket_error_reconnects_and_retries() {
        let listener = TokioTcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (messages_tx, messages_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut messages = Vec::new();
            let (first_stream, _) = listener.accept().await.unwrap();
            let mut first_ws =
                accept_hdr_async(first_stream, |_request: &ServerRequest, response| {
                    Ok(response)
                })
                .await
                .unwrap();
            let first = first_ws.next().await.unwrap().unwrap();
            let Message::Text(first) = first else {
                panic!("expected first request text frame");
            };
            messages.push(first.to_string());
            first_ws
                .send(Message::Text(
                    r#"{"type":"error","status":403,"headers":{"cf-ray":"abc123"},"error":{"message":"forbidden"}}"#
                        .into(),
                ))
                .await
                .unwrap();

            let (second_stream, _) = listener.accept().await.unwrap();
            let mut second_ws =
                accept_hdr_async(second_stream, |_request: &ServerRequest, response| {
                    Ok(response)
                })
                .await
                .unwrap();
            let second = second_ws.next().await.unwrap().unwrap();
            let Message::Text(second) = second else {
                panic!("expected retry request text frame");
            };
            messages.push(second.to_string());
            second_ws
                .send(Message::Text(
                    r#"{"type":"response.completed","response":{"id":"retry"}}"#.into(),
                ))
                .await
                .unwrap();
            messages_tx.send(messages).unwrap();
        });

        let pool = ResponsesWsPool::new(
            ApiProvider::new(format!("ws://{addr}/codex")).unwrap(),
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
            WsPoolConfig {
                min_new_connection_interval: Duration::ZERO,
                connect_timeout: Duration::from_secs(1),
                keep_connections_open: false,
                ..Default::default()
            },
        );

        pool.stream_request(
            ResponsesStreamRequest {
                headers: CodexHeaders::for_thread("session", "thread", "thread:0"),
                body: json!({"type": "response.create", "model": "gpt-test"}),
            },
            |_event| std::future::ready(()),
        )
        .await
        .unwrap();

        let messages = messages_rx.await.unwrap();
        assert_eq!(messages.len(), 2);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn non_cloudflare_forbidden_websocket_error_returns_chat_error_without_retry() {
        let listener = TokioTcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws =
                accept_hdr_async(stream, |_request: &ServerRequest, response| Ok(response))
                    .await
                    .unwrap();
            let request = ws.next().await.unwrap().unwrap();
            let Message::Text(_) = request else {
                panic!("expected request text frame");
            };
            ws.send(Message::Text(
                r#"{"type":"error","status":403,"error":{"message":"forbidden"}}"#.into(),
            ))
            .await
            .unwrap();
        });

        let pool = ResponsesWsPool::new(
            ApiProvider::new(format!("ws://{addr}/codex")).unwrap(),
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
            WsPoolConfig {
                min_new_connection_interval: Duration::ZERO,
                connect_timeout: Duration::from_secs(1),
                keep_connections_open: false,
                ..Default::default()
            },
        );

        let result = pool
            .stream_request(
                ResponsesStreamRequest {
                    headers: CodexHeaders::for_thread("session", "thread", "thread:0"),
                    body: json!({"type": "response.create", "model": "gpt-test"}),
                },
                |_event| std::future::ready(()),
            )
            .await;

        let Err(ResponsesApiError::Forbidden { cf_ray, .. }) = result else {
            panic!("expected explicit forbidden error without retry");
        };
        assert_eq!(cf_ray, None);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn initial_websocket_close_reconnects_and_retries_on_new_socket() {
        let listener = TokioTcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (messages_tx, messages_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut messages = Vec::new();
            let (first_stream, _) = listener.accept().await.unwrap();
            let mut first_ws =
                accept_hdr_async(first_stream, |_request: &ServerRequest, response| {
                    Ok(response)
                })
                .await
                .unwrap();
            let first = first_ws.next().await.unwrap().unwrap();
            let Message::Text(first) = first else {
                panic!("expected first request text frame");
            };
            messages.push(first.to_string());
            first_ws.close(None).await.unwrap();

            let (second_stream, _) = listener.accept().await.unwrap();
            let mut second_ws =
                accept_hdr_async(second_stream, |_request: &ServerRequest, response| {
                    Ok(response)
                })
                .await
                .unwrap();
            let second = second_ws.next().await.unwrap().unwrap();
            let Message::Text(second) = second else {
                panic!("expected retry request text frame");
            };
            messages.push(second.to_string());
            second_ws
                .send(Message::Text(
                    r#"{"type":"response.completed","response":{"id":"retry"}}"#.into(),
                ))
                .await
                .unwrap();
            messages_tx.send(messages).unwrap();
        });

        let pool = ResponsesWsPool::new(
            ApiProvider::new(format!("ws://{addr}/codex")).unwrap(),
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
            WsPoolConfig {
                min_new_connection_interval: Duration::ZERO,
                connect_timeout: Duration::from_secs(1),
                keep_connections_open: false,
                ..Default::default()
            },
        );

        pool.stream_request(
            ResponsesStreamRequest {
                headers: CodexHeaders::for_thread("session", "thread", "thread:0"),
                body: json!({"type": "response.create", "model": "gpt-test"}),
            },
            |_event| std::future::ready(()),
        )
        .await
        .unwrap();

        let messages = messages_rx.await.unwrap();
        assert_eq!(messages.len(), 2);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn websocket_reset_after_response_frame_is_stream_interruption() {
        let listener = TokioTcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws =
                accept_hdr_async(stream, |_request: &ServerRequest, response| Ok(response))
                    .await
                    .unwrap();
            let request = ws.next().await.unwrap().unwrap();
            let Message::Text(_) = request else {
                panic!("expected request text frame");
            };
            ws.send(Message::Text(
                r#"{"type":"response.output_text.delta","delta":"partial"}"#.into(),
            ))
            .await
            .unwrap();
        });

        let pool = ResponsesWsPool::new(
            ApiProvider::new(format!("ws://{addr}/codex")).unwrap(),
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
            WsPoolConfig {
                min_new_connection_interval: Duration::ZERO,
                connect_timeout: Duration::from_secs(1),
                ..Default::default()
            },
        );

        let result = pool
            .stream_request(
                ResponsesStreamRequest {
                    headers: CodexHeaders::for_thread("session", "thread", "thread:0"),
                    body: json!({"type": "response.create", "model": "gpt-test"}),
                },
                |_event| std::future::ready(()),
            )
            .await;

        assert!(matches!(result, Err(ResponsesApiError::StreamInterrupted)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pool_tops_up_ready_websockets_while_request_is_active_and_trims_after_grace() {
        let listener = TokioTcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (idle_tx, idle_rx) = oneshot::channel();
        let (complete_tx, complete_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws =
                accept_hdr_async(stream, |_request: &ServerRequest, response| Ok(response))
                    .await
                    .unwrap();
            let request = ws.next().await.unwrap().unwrap();
            let Message::Text(_) = request else {
                panic!("expected request text frame");
            };

            let mut idle_websockets = Vec::new();
            for _ in 0..2 {
                let (stream, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let ws =
                    accept_hdr_async(stream, |_request: &ServerRequest, response| Ok(response))
                        .await
                        .unwrap();
                idle_websockets.push(ws);
            }
            idle_tx.send(idle_websockets.len()).unwrap();
            complete_rx.await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"initial"}}"#.into(),
            ))
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            for ws in &mut idle_websockets {
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), ws.next())
                        .await
                        .is_err()
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            for ws in &mut idle_websockets {
                let message = tokio::time::timeout(Duration::from_secs(1), ws.next())
                    .await
                    .unwrap();
                assert!(matches!(message, None | Some(Ok(Message::Close(_)))));
            }
        });

        let pool = ResponsesWsPool::new(
            ApiProvider::new(format!("ws://{addr}/codex")).unwrap(),
            Auth::ApiKey("api-token".to_string()),
            lean_codex_default_headers(),
            WsPoolConfig {
                min_new_connection_interval: Duration::ZERO,
                ready_connections_per_active_connection: NonZeroUsize::new(2)
                    .expect("2 is non-zero"),
                idle_connection_grace_period: Duration::from_secs(1),
                idle_maintenance_interval: Duration::from_millis(10),
                ..Default::default()
            },
        );

        let request = tokio::spawn({
            let pool = pool.clone();
            async move {
                pool.stream_request(
                    ResponsesStreamRequest {
                        headers: CodexHeaders::for_thread("session", "thread", "thread:0"),
                        body: json!({"type": "response.create", "model": "gpt-test"}),
                    },
                    |_event| std::future::ready(()),
                )
                .await
                .unwrap();
            }
        });
        assert_eq!(idle_rx.await.unwrap(), 2);
        complete_tx.send(()).unwrap();
        request.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn managed_chatgpt_auth_refresh_uses_codex_request_and_rotates_tokens() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let refresh_url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let body_start = loop {
                let mut buffer = [0; 512];
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    break position + 4;
                }
            };
            let head = String::from_utf8(request[..body_start].to_vec()).unwrap();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    (name.eq_ignore_ascii_case("content-length"))
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while request.len() - body_start < content_length {
                let mut buffer = [0; 512];
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let body = request[body_start..body_start + content_length].to_vec();
            request_tx.send((head, body)).unwrap();

            let response_body = br#"{"access_token":"access-new","refresh_token":"refresh-new"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                response_body.len()
            )
            .unwrap();
            stream.write_all(response_body).unwrap();
        });

        let auth = ManagedChatGptAuth::with_refresh_url(
            ChatGptAuthTokens {
                access_token: "access-old".to_string(),
                refresh_token: "refresh-old".to_string(),
                account_id: Some("account".to_string()),
                fedramp: false,
            },
            refresh_url,
        );

        auth.refresh_after_unauthorized().await.unwrap();

        let tokens = auth.snapshot().unwrap();
        assert_eq!(tokens.access_token, "access-new");
        assert_eq!(tokens.refresh_token, "refresh-new");
        assert_eq!(tokens.account_id.as_deref(), Some("account"));

        let (head, body) = request_rx.recv().unwrap();
        assert!(head.starts_with("POST /oauth/token HTTP/1.1"));
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        let body = sonic_rs::from_slice::<Value>(&body).unwrap();
        let body = body.as_object().unwrap();
        assert_eq!(
            body.get(&"client_id").and_then(JsonValueTrait::as_str),
            Some(CODEX_OAUTH_CLIENT_ID)
        );
        assert_eq!(
            body.get(&"grant_type").and_then(JsonValueTrait::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            body.get(&"refresh_token").and_then(JsonValueTrait::as_str),
            Some("refresh-old")
        );
        server.join().unwrap();
    }
}
