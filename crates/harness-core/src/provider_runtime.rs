//! Provider runtime types and builder.
//!
//! A provider profile is static configuration. A [`ProviderRuntime`] is a
//! validated, ready-to-use bundle of auth, model catalog, selected model, and
//! transport driver. The harness actor should only accept a [`ProviderRuntime`],
//! never raw config.

use std::{
    collections::HashMap, fmt, future::Future, path::PathBuf, pin::Pin, sync::Arc, time::SystemTime,
};

use http::HeaderMap;
use serde::{Deserialize, Serialize};
use sonic_rs::JsonValueTrait;
use thiserror::Error;
use url::Url;

use crate::{
    compact::ContextWindowPolicy,
    providers::{
        ProviderAuthConfig, ProviderKind, ProviderModelConfig, ProviderProfile, ProviderProfileId,
        ProviderTransportKind, ProviderUiInfo,
    },
    responses::{
        ApiProvider, Auth, ChatGptAuthSession, ManagedChatGptAuth, ModelSettings,
        OllamaModelInfoClient, ResponsesApiError, ResponsesModelCapabilities, ResponsesModelInfo,
        ResponsesModelsClient, ResponsesModelsList, context_window_policy_from_model_info,
        lean_codex_default_headers, normalize_model_slug,
    },
    tools::NativeToolRegistry,
};

/// Errors that can occur while building or using a provider runtime.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Provider base URL could not be parsed.
    #[error("invalid provider base URL `{url}`: {source}")]
    InvalidBaseUrl {
        /// Base URL string that failed parsing.
        url: String,
        /// URL parse error.
        #[source]
        source: url::ParseError,
    },
    /// Provider base URL includes a query or fragment.
    #[error("provider base URL `{url}` must not include query or fragment")]
    BaseUrlHasQueryOrFragment { url: String },
    /// Auth resolution failed.
    #[error("failed to resolve provider auth: {0}")]
    Auth(String),
    /// Model catalog fetch failed.
    #[error("failed to fetch model catalog: {0}")]
    ModelCatalog(#[from] ResponsesApiError),
    /// Selected model is not available from the provider.
    #[error("model `{model}` is not available from provider `{provider}`")]
    MissingModel {
        /// Provider display name.
        provider: String,
        /// Model slug that is missing.
        model: String,
    },
    /// Selected model lacks required capability metadata.
    #[error("model `{model}` lacks required capability metadata: {reason}")]
    IncompleteModelCapabilities {
        /// Model slug.
        model: String,
        /// Reason the model cannot be used.
        reason: String,
    },
    /// The configured model slug is empty or otherwise invalid.
    #[error("invalid model selection for provider `{provider}`: {reason}")]
    InvalidModelSelection {
        /// Provider display name.
        provider: String,
        /// Human-readable reason.
        reason: String,
    },
    /// HTTP transport is not supported by the current driver.
    #[error("transport error: {0}")]
    Transport(String),
    /// The provider driver kind is not supported yet.
    #[error("unsupported provider driver for `{profile_id}`")]
    UnsupportedDriver {
        /// Provider profile id.
        profile_id: ProviderProfileId,
    },
}

/// Binding between a session and the provider profile used for its turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSessionBinding {
    /// Provider profile id used by the session.
    pub profile_id: ProviderProfileId,
    /// Provider kind used when the binding was created.
    pub kind: ProviderKind,
    /// Selected model and request settings.
    pub model_settings: ModelSettings,
}

impl ProviderSessionBinding {
    /// Build a session binding from a validated runtime.
    pub fn from_runtime(runtime: &ProviderRuntime) -> Self {
        Self {
            profile_id: runtime.profile.id.clone(),
            kind: runtime.profile.kind,
            model_settings: runtime.selected_model.clone(),
        }
    }
}

/// Snapshot of provider state persisted with each turn for historical clarity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTurnSnapshot {
    /// Provider profile id.
    pub profile_id: ProviderProfileId,
    /// Provider display name.
    pub display_name: String,
    /// Provider kind.
    pub kind: ProviderKind,
    /// Transport kind actually used.
    pub transport: ProviderTransportKind,
    /// Base URL used for the turn.
    pub base_url: String,
    /// Model slug used for the turn.
    pub model: String,
}

impl ProviderTurnSnapshot {
    /// Build a turn snapshot from a runtime.
    pub fn from_runtime(runtime: &ProviderRuntime) -> Self {
        Self {
            profile_id: runtime.profile.id.clone(),
            display_name: runtime.profile.display_name.clone(),
            kind: runtime.profile.kind,
            transport: runtime.profile.driver.transport(),
            base_url: runtime.profile.driver.base_url().to_string(),
            model: runtime.selected_model.model.clone(),
        }
    }
}

/// One model entry in a provider-scoped catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModelEntry {
    /// Raw model metadata returned by the provider.
    pub info: ResponsesModelInfo,
    /// Locally configured capability metadata, when present.
    pub config: Option<ProviderModelConfig>,
    /// Effective capabilities for this model in this provider.
    pub capabilities: ResponsesModelCapabilities,
}

/// Provider-scoped model catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModelCatalog {
    /// Profile id that produced this catalog.
    pub profile_id: ProviderProfileId,
    /// Models returned by the provider.
    pub models: Vec<ProviderModelEntry>,
    /// ETag from the provider response, if any.
    pub etag: Option<String>,
    /// When the catalog was fetched.
    pub fetched_at: SystemTime,
}

impl ProviderModelCatalog {
    /// Look up a model entry by slug.
    pub fn entry(&self, slug: &str) -> Option<&ProviderModelEntry> {
        self.models.iter().find(|entry| entry.info.slug == slug)
    }

    /// Return the underlying raw model info list.
    pub fn raw_models(&self) -> Vec<ResponsesModelInfo> {
        self.models.iter().map(|entry| entry.info.clone()).collect()
    }
}

/// Resolved auth state for a provider runtime.
#[derive(Clone)]
pub enum ProviderAuthRuntime {
    /// ChatGPT auth with a resolved session.
    ChatGpt(Arc<ManagedChatGptAuth>),
    /// API key auth.
    ApiKey(String),
}

impl std::fmt::Debug for ProviderAuthRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChatGpt(_) => f.debug_tuple("ChatGpt").finish(),
            Self::ApiKey(_) => f.debug_tuple("ApiKey").finish(),
        }
    }
}

impl ProviderAuthRuntime {
    /// Return the bearer access token if this is ChatGPT auth.
    pub fn access_token(&self) -> Option<String> {
        match self {
            Self::ChatGpt(auth) => Some(
                auth.access_token()
                    .expect("managed ChatGPT auth must have an access token"),
            ),
            Self::ApiKey(_) => None,
        }
    }

    /// Convert into the low-level [`Auth`] enum used by the Responses API layer.
    pub fn into_responses_auth(self) -> Auth {
        match self {
            Self::ChatGpt(auth) => Auth::ChatGpt(auth),
            Self::ApiKey(key) => Auth::ApiKey(key),
        }
    }
}

/// A validated, ready-to-use provider runtime.
///
/// Invariants enforced by construction via [`ProviderRuntimeBuilder`]:
/// - Base URL is valid and has no query/fragment.
/// - Auth is resolved.
/// - Model catalog is fetched.
/// - Selected model exists in the catalog.
/// - Model capabilities are sufficient for the harness.
#[derive(Debug, Clone)]
pub struct ProviderRuntime {
    /// Source profile.
    pub profile: ProviderProfile,
    /// Low-level URL/header primitive.
    pub api: ApiProvider,
    /// Default headers sent with every request.
    pub default_headers: HeaderMap,
    /// Resolved auth.
    pub auth: ProviderAuthRuntime,
    /// Provider-scoped model catalog.
    pub model_catalog: ProviderModelCatalog,
    /// Selected model and execution settings.
    pub selected_model: ModelSettings,
    /// Effective model capabilities for the selected model.
    pub selected_capabilities: ResponsesModelCapabilities,
    /// Effective context-window policy for the selected model.
    pub context_window_policy: ContextWindowPolicy,
}

impl ProviderRuntime {
    /// Return a UI summary for this runtime.
    pub fn ui_info(&self) -> ProviderUiInfo {
        ProviderUiInfo::from_profile(&self.profile)
    }

    /// Return a compact provider/transport label.
    pub fn compact_label(&self) -> String {
        self.ui_info().compact_label()
    }

    /// Return a turn snapshot.
    pub fn turn_snapshot(&self) -> ProviderTurnSnapshot {
        ProviderTurnSnapshot::from_runtime(self)
    }

    /// Return a session binding.
    pub fn session_binding(&self) -> ProviderSessionBinding {
        ProviderSessionBinding::from_runtime(self)
    }

    /// Replace the selected model settings and revalidate against the catalog.
    pub fn with_model_settings(&self, settings: ModelSettings) -> Result<Self, ProviderError> {
        ProviderRuntimeBuilder::new(self.profile.clone())
            .with_auth(self.auth.clone())
            .with_model_catalog(self.model_catalog.clone())
            .select_model(settings)
    }
}

/// Builds a validated [`ProviderRuntime`] from a profile.
#[derive(Clone)]
pub struct ProviderRuntimeBuilder {
    profile: ProviderProfile,
    auth: Option<ProviderAuthRuntime>,
    model_catalog: Option<ProviderModelCatalog>,
    selected_model: Option<ModelSettings>,
    codex_home: Option<PathBuf>,
    credential_store: Option<Arc<dyn ProviderCredentialStore>>,
}

impl std::fmt::Debug for ProviderRuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRuntimeBuilder")
            .field("profile", &self.profile)
            .field("auth", &self.auth)
            .field("model_catalog", &self.model_catalog)
            .field("selected_model", &self.selected_model)
            .field("codex_home", &self.codex_home)
            .field(
                "credential_store",
                &self.credential_store.as_ref().map(|_| "..."),
            )
            .finish()
    }
}

impl ProviderRuntimeBuilder {
    /// Start building a runtime for the given profile.
    pub fn new(profile: ProviderProfile) -> Self {
        Self {
            profile,
            auth: None,
            model_catalog: None,
            selected_model: None,
            codex_home: None,
            credential_store: None,
        }
    }

    /// Provide a resolved auth override.
    pub fn with_auth(mut self, auth: ProviderAuthRuntime) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Provide a pre-fetched model catalog.
    pub fn with_model_catalog(mut self, catalog: ProviderModelCatalog) -> Self {
        self.model_catalog = Some(catalog);
        self
    }

    /// Set the Codex home path used to load ChatGPT auth.
    pub fn with_codex_home(mut self, codex_home: impl Into<PathBuf>) -> Self {
        self.codex_home = Some(codex_home.into());
        self
    }

    /// Set the credential store used to load API keys.
    pub fn with_credential_store(mut self, store: Arc<dyn ProviderCredentialStore>) -> Self {
        self.credential_store = Some(store);
        self
    }

    /// Use the profile's default model selection.
    pub fn select_default_model(self) -> Result<ProviderRuntime, ProviderError> {
        let settings = ModelSettings::new(
            self.profile.default_model.clone(),
            self.profile.default_reasoning_effort.clone(),
            self.profile.default_service_tier.clone(),
        );
        self.select_model(settings)
    }

    /// Validate and select the given model settings.
    pub fn select_model(
        mut self,
        settings: ModelSettings,
    ) -> Result<ProviderRuntime, ProviderError> {
        let normalized = normalize_model_slug(&settings.model);
        if normalized.is_empty() {
            return Err(ProviderError::InvalidModelSelection {
                provider: self.profile.display_name.clone(),
                reason: "model slug is empty".to_string(),
            });
        }
        self.selected_model = Some(ModelSettings::new(
            normalized,
            settings.reasoning_effort,
            settings.service_tier,
        ));
        self.build()
    }

    /// Build the runtime using the profile's default model, fetching anything
    /// that was not pre-supplied.
    pub async fn build_async(self) -> Result<ProviderRuntime, ProviderError> {
        self.build_async_with_model(None).await
    }

    /// Build the runtime using an explicit model selection, fetching anything
    /// that was not pre-supplied.
    pub async fn build_async_with_model(
        mut self,
        settings: Option<ModelSettings>,
    ) -> Result<ProviderRuntime, ProviderError> {
        if let Some(settings) = settings {
            let normalized = normalize_model_slug(&settings.model);
            if normalized.is_empty() {
                return Err(ProviderError::InvalidModelSelection {
                    provider: self.profile.display_name.clone(),
                    reason: "model slug is empty".to_string(),
                });
            }
            self.selected_model = Some(ModelSettings::new(
                normalized,
                settings.reasoning_effort,
                settings.service_tier,
            ));
        }
        let auth = self.resolve_auth().await?;
        self.auth = Some(auth);
        if self.model_catalog.is_none() {
            let auth = self.auth.as_ref().expect("auth just set").clone();
            let catalog = match self.profile.kind {
                ProviderKind::HttpsApi => Ok(self.build_catalog_from_profile_configs()),
                ProviderKind::Codex | ProviderKind::OllamaCloud => {
                    self.fetch_model_catalog(&auth).await
                }
            };
            self.model_catalog = Some(catalog?);
        }
        self.build()
    }

    fn build_catalog_from_profile_configs(&self) -> ProviderModelCatalog {
        let models = self
            .profile
            .model_configs
            .iter()
            .map(|config| {
                let info = crate::responses::ResponsesModelInfo {
                    slug: config.slug.clone(),
                    supports_parallel_tool_calls: config.supports_parallel_tool_calls,
                    supports_reasoning: false,
                    context_window: config.context_window,
                    max_context_window: config.context_window,
                    auto_compact_token_limit: None,
                    effective_context_window_percent: config.effective_context_window_percent,
                };
                ProviderModelEntry {
                    info: info.clone(),
                    config: Some(config.clone()),
                    capabilities: merge_capabilities(&info, Some(config)),
                }
            })
            .collect();
        ProviderModelCatalog {
            profile_id: self.profile.id.clone(),
            models,
            etag: None,
            fetched_at: std::time::SystemTime::now(),
        }
    }

    fn build(mut self) -> Result<ProviderRuntime, ProviderError> {
        let api = build_api_provider(&self.profile)?;
        let auth = self
            .auth
            .take()
            .ok_or_else(|| ProviderError::Auth("auth not resolved".to_string()))?;
        let default_headers = default_headers_for_profile(&self.profile);
        let catalog = self
            .model_catalog
            .take()
            .ok_or_else(|| ProviderError::Transport("model catalog missing".to_string()))?;
        let selected_model = self.selected_model.take().unwrap_or_else(|| {
            ModelSettings::new(
                self.profile.default_model.clone(),
                self.profile.default_reasoning_effort.clone(),
                self.profile.default_service_tier.clone(),
            )
        });

        let entry = catalog
            .entry(&selected_model.model)
            .ok_or_else(|| ProviderError::MissingModel {
                provider: self.profile.display_name.clone(),
                model: selected_model.model.clone(),
            })?
            .clone();

        let supports_tools = entry.config.as_ref().map_or(true, |c| c.supports_tools);
        if !supports_tools {
            return Err(ProviderError::IncompleteModelCapabilities {
                model: selected_model.model.clone(),
                reason: "model does not report tool support".to_string(),
            });
        }

        let context_window_policy =
            context_window_policy_from_model_info(&entry.info).map_err(|_| {
                ProviderError::IncompleteModelCapabilities {
                    model: selected_model.model.clone(),
                    reason: "model does not report a usable context window".to_string(),
                }
            })?;

        Ok(ProviderRuntime {
            profile: self.profile,
            api,
            default_headers,
            auth,
            model_catalog: catalog,
            selected_model,
            selected_capabilities: entry.capabilities,
            context_window_policy,
        })
    }

    async fn resolve_auth(&self) -> Result<ProviderAuthRuntime, ProviderError> {
        match &self.profile.auth {
            ProviderAuthConfig::ChatGptHarness => {
                let codex_home = self.codex_home.clone().unwrap_or_else(default_codex_home);
                let tokens = load_chatgpt_tokens(&codex_home)
                    .await
                    .map_err(|message| ProviderError::Auth(message))?;
                Ok(ProviderAuthRuntime::ChatGpt(Arc::new(
                    ManagedChatGptAuth::new(tokens),
                )))
            }
            ProviderAuthConfig::ApiKey { credential_id } => {
                let Some(store) = self.credential_store.as_ref() else {
                    return Err(ProviderError::Auth(
                        "no credential store configured".to_string(),
                    ));
                };
                let key = store
                    .load_api_key(credential_id)
                    .await
                    .map_err(|err| ProviderError::Auth(err.to_string()))?;
                Ok(ProviderAuthRuntime::ApiKey(key))
            }
        }
    }

    async fn fetch_model_catalog(
        &self,
        auth: &ProviderAuthRuntime,
    ) -> Result<ProviderModelCatalog, ProviderError> {
        let api = build_api_provider(&self.profile)?;
        let responses_auth = auth.clone().into_responses_auth();
        let default_headers = default_headers_for_profile(&self.profile);
        let client = ResponsesModelsClient::new(
            api.clone(),
            responses_auth.clone(),
            default_headers.clone(),
        );
        let list = client.list_models().await.map_err(|error| {
            eprintln!("warning: failed to fetch model catalog from provider: {error}");
            error
        })?;
        let list = match self.profile.kind {
            ProviderKind::OllamaCloud => {
                enrich_ollama_model_catalog(api, responses_auth, default_headers, list).await?
            }
            ProviderKind::Codex | ProviderKind::HttpsApi => list,
        };
        let etag = list.etag.clone();
        let models = build_provider_model_entries(&self.profile, &list);
        Ok(ProviderModelCatalog {
            profile_id: self.profile.id.clone(),
            models,
            etag,
            fetched_at: SystemTime::now(),
        })
    }
}

fn build_api_provider(profile: &ProviderProfile) -> Result<ApiProvider, ProviderError> {
    let url =
        Url::parse(profile.driver.base_url()).map_err(|source| ProviderError::InvalidBaseUrl {
            url: profile.driver.base_url().to_string(),
            source,
        })?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ProviderError::BaseUrlHasQueryOrFragment {
            url: profile.driver.base_url().to_string(),
        });
    }
    let api = ApiProvider::new(profile.driver.base_url())
        .map_err(|err| ProviderError::Transport(err.to_string()))?
        .with_stream_idle_timeout(profile.driver.stream_idle_timeout());
    Ok(api)
}

fn default_headers_for_profile(profile: &ProviderProfile) -> HeaderMap {
    match profile.kind {
        ProviderKind::Codex => lean_codex_default_headers(),
        ProviderKind::OllamaCloud | ProviderKind::HttpsApi => HeaderMap::new(),
    }
}

async fn enrich_ollama_model_catalog(
    api: ApiProvider,
    auth: Auth,
    default_headers: HeaderMap,
    mut list: ResponsesModelsList,
) -> Result<ResponsesModelsList, ResponsesApiError> {
    let client = OllamaModelInfoClient::new(api, auth, default_headers);
    for model in &mut list.models {
        if model.resolved_context_window().is_some() {
            continue;
        }
        if let Some(info) = client.model_info(&model.slug).await? {
            model.context_window = info.context_window;
            model.max_context_window = info.max_context_window;
            model.effective_context_window_percent = info.effective_context_window_percent;
        }
    }
    Ok(list)
}

fn build_provider_model_entries(
    profile: &ProviderProfile,
    list: &ResponsesModelsList,
) -> Vec<ProviderModelEntry> {
    let configs_by_slug: HashMap<String, ProviderModelConfig> = profile
        .model_configs
        .iter()
        .map(|config| (config.slug.clone(), config.clone()))
        .collect();

    list.models
        .iter()
        .map(|info| {
            let config = configs_by_slug.get(&info.slug).cloned();
            let merged_info = merge_model_info(info, config.as_ref());
            let capabilities = merge_capabilities(&merged_info, config.as_ref());
            ProviderModelEntry {
                info: merged_info,
                config,
                capabilities,
            }
        })
        .collect()
}

fn merge_model_info(
    info: &ResponsesModelInfo,
    config: Option<&ProviderModelConfig>,
) -> ResponsesModelInfo {
    let Some(config) = config else {
        return info.clone();
    };
    ResponsesModelInfo {
        slug: info.slug.clone(),
        supports_parallel_tool_calls: info.supports_parallel_tool_calls
            || config.supports_parallel_tool_calls,
        supports_reasoning: info.supports_reasoning,
        context_window: info.context_window.or(config.context_window),
        max_context_window: info.max_context_window.or(config.context_window),
        auto_compact_token_limit: info.auto_compact_token_limit,
        effective_context_window_percent: if info.effective_context_window_percent == 95 {
            config.effective_context_window_percent
        } else {
            info.effective_context_window_percent
        },
    }
}

fn merge_capabilities(
    info: &ResponsesModelInfo,
    config: Option<&ProviderModelConfig>,
) -> ResponsesModelCapabilities {
    let supports_parallel = if info.slug == crate::responses::DEFAULT_MODEL {
        true
    } else {
        info.supports_parallel_tool_calls
            || config.map_or(false, |c| c.supports_parallel_tool_calls)
    };
    let supports_reasoning = if info.slug == crate::responses::DEFAULT_MODEL {
        true
    } else {
        info.supports_reasoning
    };
    ResponsesModelCapabilities::new(supports_parallel, supports_reasoning)
}
async fn load_chatgpt_tokens(
    codex_home: &std::path::Path,
) -> Result<harness_responses_api::ChatGptAuthTokens, String> {
    let paths = [
        codex_home.join("auth.json"),
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
            .join(".codex")
            .join("auth.json"),
    ];
    for path in paths {
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            let value: sonic_rs::Value = sonic_rs::from_str(&text)
                .map_err(|err| format!("failed to decode auth file {}: {}", path.display(), err))?;
            let tokens = chatgpt_tokens_from_auth_json(&value)
                .ok_or_else(|| format!("missing tokens.access_token in {}", path.display()))?;
            return Ok(tokens);
        }
    }
    Err("ChatGPT auth file not found".to_string())
}

fn chatgpt_tokens_from_auth_json(
    value: &sonic_rs::Value,
) -> Option<harness_responses_api::ChatGptAuthTokens> {
    let tokens = &value["tokens"];
    if tokens.is_object() {
        let access_token = non_empty_string(&tokens["access_token"])?.to_string();
        let refresh_token = tokens["refresh_token"].as_str().unwrap_or("").to_string();
        let account_id = non_empty_string(&tokens["account_id"]).map(str::to_string);
        let fedramp = tokens["fedramp"].as_bool().unwrap_or(false);
        return Some(harness_responses_api::ChatGptAuthTokens {
            access_token,
            refresh_token,
            account_id,
            fedramp,
        });
    }

    let access_token = non_empty_string(&value["access_token"])?.to_string();
    let refresh_token = value["refresh_token"].as_str().unwrap_or("").to_string();
    let account_id = non_empty_string(&value["account_id"]).map(str::to_string);
    let fedramp = value["fedramp"].as_bool().unwrap_or(false);
    Some(harness_responses_api::ChatGptAuthTokens {
        access_token,
        refresh_token,
        account_id,
        fedramp,
    })
}

fn non_empty_string(value: &sonic_rs::Value) -> Option<&str> {
    let text = value.as_str()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn default_codex_home() -> PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
        .join(".codex")
}

/// Error produced by the credential store.
#[derive(Debug, Error)]
pub enum ProviderCredentialError {
    /// Credential read failed.
    #[error("credential read failed: {0}")]
    Read(String),
    /// Credential write failed.
    #[error("credential write failed: {0}")]
    Write(String),
}

/// Storage backend for provider API-key credentials.
pub trait ProviderCredentialStore: Send + Sync {
    /// Load an API key by credential id.
    fn load_api_key(
        &self,
        credential_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderCredentialError>> + Send + '_>>;
    /// Save a new API key and return its credential id.
    fn save_api_key(
        &self,
        label: String,
        key: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderCredentialError>> + Send + '_>>;
}

/// File-backed credential store.
#[derive(Debug, Clone)]
pub struct FileProviderCredentialStore {
    root: PathBuf,
}

impl FileProviderCredentialStore {
    /// Create a file-backed credential store.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl ProviderCredentialStore for FileProviderCredentialStore {
    fn load_api_key(
        &self,
        credential_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderCredentialError>> + Send + '_>> {
        let path = self.root.join("provider-credentials").join(credential_id);
        Box::pin(async move {
            tokio::fs::read_to_string(&path)
                .await
                .map(|text| text.trim().to_string())
                .map_err(|err| ProviderCredentialError::Read(err.to_string()))
        })
    }

    fn save_api_key(
        &self,
        _label: String,
        key: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderCredentialError>> + Send + '_>> {
        let dir = self.root.join("provider-credentials");
        Box::pin(async move {
            let id = format!("cred_{}", uuid::Uuid::new_v4().simple());
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|err| ProviderCredentialError::Write(err.to_string()))?;
            let path = dir.join(&id);
            tokio::fs::write(&path, key)
                .await
                .map_err(|err| ProviderCredentialError::Write(err.to_string()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await;
            }
            Ok(id)
        })
    }
}

/// Result of testing a provider profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTestReport {
    /// Profile id tested.
    pub profile_id: ProviderProfileId,
    /// Steps executed.
    pub steps: Vec<ProviderTestStep>,
}

/// One step in a provider test report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTestStep {
    /// Human-readable label.
    pub label: String,
    /// Step status.
    pub status: ProviderTestStepStatus,
    /// Optional details.
    pub details: Option<String>,
}

/// Status of a provider test step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTestStepStatus {
    /// Step passed.
    Passed,
    /// Step failed.
    Failed,
    /// Step was skipped.
    Skipped,
}

/// Mode for testing a provider profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTestMode {
    /// Basic readiness checks (URL, auth, model catalog).
    Basic,
    /// Full checks including a live request.
    Full,
}

impl fmt::Display for ProviderTestStepStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Passed => f.write_str("passed"),
            Self::Failed => f.write_str("failed"),
            Self::Skipped => f.write_str("skipped"),
        }
    }
}

/// Native tool registry appropriate for a provider kind.
pub fn tool_registry_for_provider(kind: ProviderKind) -> NativeToolRegistry {
    match kind {
        ProviderKind::OllamaCloud => NativeToolRegistry::ollama_cloud(),
        ProviderKind::Codex | ProviderKind::HttpsApi => NativeToolRegistry::codex(),
    }
}

/// Test a provider profile and return a structured report.
pub async fn test_provider_profile(
    profile: &ProviderProfile,
    mode: ProviderTestMode,
    codex_home: Option<PathBuf>,
    credential_store: Option<Arc<dyn ProviderCredentialStore>>,
) -> ProviderTestReport {
    let mut steps = Vec::new();

    let api_result = build_api_provider(profile);
    match api_result {
        Ok(_) => steps.push(ProviderTestStep {
            label: "Base URL valid".to_string(),
            status: ProviderTestStepStatus::Passed,
            details: None,
        }),
        Err(err) => steps.push(ProviderTestStep {
            label: "Base URL valid".to_string(),
            status: ProviderTestStepStatus::Failed,
            details: Some(err.to_string()),
        }),
    }

    let mut builder = ProviderRuntimeBuilder::new(profile.clone());
    if let Some(codex_home) = codex_home {
        builder = builder.with_codex_home(codex_home);
    }
    if let Some(store) = credential_store {
        builder = builder.with_credential_store(store);
    }

    match builder.resolve_auth().await {
        Ok(_) => steps.push(ProviderTestStep {
            label: "Auth available".to_string(),
            status: ProviderTestStepStatus::Passed,
            details: None,
        }),
        Err(err) => steps.push(ProviderTestStep {
            label: "Auth available".to_string(),
            status: ProviderTestStepStatus::Failed,
            details: Some(err.to_string()),
        }),
    }

    let catalog_result = async {
        let auth = builder.resolve_auth().await?;
        builder.fetch_model_catalog(&auth).await
    }
    .await;

    match catalog_result {
        Ok(catalog) => {
            steps.push(ProviderTestStep {
                label: "Model catalog loaded".to_string(),
                status: ProviderTestStepStatus::Passed,
                details: Some(format!("{} models", catalog.models.len())),
            });
            let default_ok = profile
                .default_model
                .as_str()
                .is_empty()
                .not()
                .then(|| catalog.entry(&profile.default_model).is_some());
            if let Some(true) = default_ok {
                steps.push(ProviderTestStep {
                    label: "Default model available".to_string(),
                    status: ProviderTestStepStatus::Passed,
                    details: Some(profile.default_model.clone()),
                });
            }
        }
        Err(err) => steps.push(ProviderTestStep {
            label: "Model catalog loaded".to_string(),
            status: ProviderTestStepStatus::Failed,
            details: Some(err.to_string()),
        }),
    }

    if matches!(mode, ProviderTestMode::Full) {
        steps.push(ProviderTestStep {
            label: "Live request test".to_string(),
            status: ProviderTestStepStatus::Skipped,
            details: Some("not yet implemented".to_string()),
        });
    }

    ProviderTestReport {
        profile_id: profile.id.clone(),
        steps,
    }
}

trait BoolExt {
    fn not(self) -> bool;
}

impl BoolExt for bool {
    fn not(self) -> bool {
        !self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ProviderAuthConfig, ProviderDriverConfig};

    #[test]
    fn chatgpt_tokens_from_auth_json_reads_codex_tokens_shape() {
        let value: sonic_rs::Value = sonic_rs::from_str(
            r#"{"tokens":{"access_token":"codex-access","account_id":"acct-1"}}"#,
        )
        .unwrap();

        let tokens = chatgpt_tokens_from_auth_json(&value).unwrap();

        assert_eq!(tokens.access_token, "codex-access");
        assert_eq!(tokens.refresh_token, "");
        assert_eq!(tokens.account_id.as_deref(), Some("acct-1"));
        assert!(!tokens.fedramp);
    }

    #[test]
    fn chatgpt_tokens_from_auth_json_reads_flat_auth_shape() {
        let value: sonic_rs::Value = sonic_rs::from_str(
            r#"{"access_token":"flat-access","refresh_token":"refresh","account_id":"acct-2","fedramp":true}"#,
        )
        .unwrap();

        let tokens = chatgpt_tokens_from_auth_json(&value).unwrap();

        assert_eq!(tokens.access_token, "flat-access");
        assert_eq!(tokens.refresh_token, "refresh");
        assert_eq!(tokens.account_id.as_deref(), Some("acct-2"));
        assert!(tokens.fedramp);
    }

    #[test]
    fn chatgpt_tokens_from_auth_json_rejects_blank_codex_access_token() {
        let value: sonic_rs::Value =
            sonic_rs::from_str(r#"{"tokens":{"access_token":"   "}}"#).unwrap();

        assert!(chatgpt_tokens_from_auth_json(&value).is_none());
    }

    #[tokio::test]
    async fn ollama_cloud_runtime_uses_native_context_window_for_glm_models() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.readable().await;
                assert!(read.is_ok());
                match stream.try_read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buffer[..n]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(err) => panic!("failed to read request: {err}"),
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /v1/models?client_version="));
            assert!(request.contains("authorization: Bearer ollama-token\r\n"));

            let body = r#"{"object":"list","data":[{"id":"glm-5.2","object":"model","created":1754352000,"owned_by":"ollama"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.writable().await.unwrap();
            stream.try_write(response.as_bytes()).unwrap();

            let (stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.readable().await;
                assert!(read.is_ok());
                match stream.try_read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buffer[..n]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(err) => panic!("failed to read request: {err}"),
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("POST /api/show "));
            assert!(request.contains("authorization: Bearer ollama-token\r\n"));

            let body =
                r#"{"model_info":{"general.architecture":"glm","glm.context_length":131072}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.writable().await.unwrap();
            stream.try_write(response.as_bytes()).unwrap();
        });

        let profile = ProviderProfile {
            id: ProviderProfileId::new("ollama-cloud"),
            display_name: "Ollama Cloud".to_string(),
            kind: ProviderKind::OllamaCloud,
            auth: ProviderAuthConfig::ApiKey {
                credential_id: "cred-test".to_string(),
            },
            driver: ProviderDriverConfig::HttpsResponses {
                base_url: format!("http://{addr}/v1"),
                request_timeout_ms: 300_000,
                stream_idle_timeout_ms: 300_000,
            },
            default_model: "glm-5.2".to_string(),
            default_reasoning_effort: None,
            default_service_tier: None,
            model_configs: Vec::new(),
            tool_output_summary_model: "glm-5.2".to_string(),
        };
        let credential_store = Arc::new(TestCredentialStore {
            credential_id: "cred-test".to_string(),
            key: "ollama-token".to_string(),
        });

        let runtime = ProviderRuntimeBuilder::new(profile)
            .with_credential_store(credential_store)
            .build_async()
            .await
            .unwrap();

        assert_eq!(runtime.selected_model.model, "glm-5.2");
        assert_eq!(runtime.model_catalog.models.len(), 1);
        assert_eq!(
            runtime.model_catalog.models[0].info.context_window,
            Some(131_072)
        );
        assert_eq!(runtime.context_window_policy.max_input_tokens, 124_518);
        assert_eq!(runtime.context_window_policy.compact_at_tokens, 118_292);
        server.await.unwrap();
    }

    #[derive(Debug)]
    struct TestCredentialStore {
        credential_id: String,
        key: String,
    }

    impl ProviderCredentialStore for TestCredentialStore {
        fn load_api_key(
            &self,
            credential_id: &str,
        ) -> Pin<Box<dyn Future<Output = Result<String, ProviderCredentialError>> + Send + '_>>
        {
            assert_eq!(credential_id, self.credential_id);
            Box::pin(async { Ok(self.key.clone()) })
        }

        fn save_api_key(
            &self,
            _label: String,
            _key: String,
        ) -> Pin<Box<dyn Future<Output = Result<String, ProviderCredentialError>> + Send + '_>>
        {
            panic!("test credential store does not save keys")
        }
    }
}
