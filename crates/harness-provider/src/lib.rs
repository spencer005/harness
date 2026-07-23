//! Provider resolution and switching contracts.
//!
//! This crate owns provider-neutral resolution policy. Credential loading,
//! model catalogs, and network transports are injected through ports.

use harness_model_api::{
    ContextLimits, ModelCapabilities, ModelSelection, ModelTransport, ProviderId,
    ResolvedModelRoute,
};
use thiserror::Error;

/// Explicit routes available after provider resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRoutes {
    /// Route for ordinary root conversation attempts.
    pub root: ResolvedModelRoute,
    /// Route for compaction requests.
    pub compaction: ResolvedModelRoute,
    /// Optional route for tool-output summaries.
    pub tool_summary: Option<ResolvedModelRoute>,
    /// Optional route for locator requests.
    pub locator: Option<ResolvedModelRoute>,
}

/// Fully resolved provider state.
pub struct ResolvedProvider {
    /// Stable provider identity.
    pub identity: ProviderIdentity,
    /// Provider generation assigned by composition.
    pub generation: ProviderGeneration,
    /// Selected model and settings.
    pub selected_model: ModelSelection,
    /// Capabilities of the selected model.
    pub capabilities: ModelCapabilities,
    /// Context limits of the selected model.
    pub context_limits: ContextLimits,
    /// Explicitly available routes.
    pub routes: ProviderRoutes,
    /// Transport for the resolved provider.
    pub transport: Box<dyn ModelTransport>,
}

/// Provider selection persisted by the session/configuration store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSelection {
    /// Stable provider identity.
    pub provider: ProviderId,
    /// Provider generation committed by the controller.
    pub generation: ProviderGeneration,
    /// Selected model and settings.
    pub model: ModelSelection,
}

impl ResolvedProvider {
    /// Assigns one provider generation to the provider and every route.
    pub fn with_generation(mut self, generation: ProviderGeneration) -> Self {
        self.generation = generation;
        self.routes.root.generation = generation;
        self.routes.compaction.generation = generation;
        if let Some(route) = &mut self.routes.tool_summary {
            route.generation = generation;
        }
        if let Some(route) = &mut self.routes.locator {
            route.generation = generation;
        }
        self
    }
}

impl std::fmt::Debug for ResolvedProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedProvider")
            .field("identity", &self.identity)
            .field("selected_model", &self.selected_model)
            .field("capabilities", &self.capabilities)
            .field("context_limits", &self.context_limits)
            .field("routes", &self.routes)
            .finish_non_exhaustive()
    }
}

/// Provider identity shown in runtime state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderIdentity {
    /// Stable provider identifier.
    pub id: ProviderId,
    /// User-facing display name.
    pub display_name: String,
    /// User-facing transport label.
    pub transport: String,
}

pub use harness_model_api::ProviderGeneration;
/// State of a transactional provider switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSwitchPhase {
    /// A switch is requested but not started.
    Requested,
    /// Credentials are being resolved.
    ResolvingCredentials,
    /// The provider model catalog is loading.
    LoadingCatalog,
    /// The selected model is validated against the catalog.
    ValidatingModel,
    /// The new transport is starting.
    StartingTransport,
    /// The provider selection is being persisted.
    PersistingSelection,
    /// The new provider is active.
    Ready,
    /// The switch fails while the previous provider remains active.
    Failed { message: String },
}

/// Coordinates transactional provider replacement.
pub struct ProviderController {
    active: Option<ResolvedProvider>,
    next_generation: u64,
    phase: ProviderSwitchPhase,
    resolver: std::sync::Arc<dyn ProviderResolver>,
    selection_store: std::sync::Arc<dyn ProviderSelectionStore>,
}

impl ProviderController {
    /// Creates a controller with no active provider.
    pub fn new(
        resolver: std::sync::Arc<dyn ProviderResolver>,
        selection_store: std::sync::Arc<dyn ProviderSelectionStore>,
    ) -> Self {
        Self {
            active: None,
            next_generation: 0,
            phase: ProviderSwitchPhase::Requested,
            resolver,
            selection_store,
        }
    }

    /// Resolves and commits a provider transactionally.
    pub async fn switch(
        &mut self,
        selection: ModelSelection,
    ) -> Result<ProviderGeneration, ProviderError> {
        self.phase = ProviderSwitchPhase::ResolvingCredentials;
        let mut provider = match self.resolver.resolve(selection).await {
            Ok(provider) => provider,
            Err(error) => {
                self.phase = ProviderSwitchPhase::Failed {
                    message: error.to_string(),
                };
                return Err(error);
            }
        };
        self.phase = ProviderSwitchPhase::LoadingCatalog;
        self.phase = ProviderSwitchPhase::ValidatingModel;
        self.phase = ProviderSwitchPhase::StartingTransport;

        let generation = self
            .next_generation
            .checked_add(1)
            .ok_or(ProviderError::GenerationExhausted)?;
        let generation = ProviderGeneration(generation);
        provider = provider.with_generation(generation);

        self.phase = ProviderSwitchPhase::PersistingSelection;
        let persisted = ProviderSelection {
            provider: provider.identity.id.clone(),
            generation,
            model: provider.selected_model.clone(),
        };
        if let Err(error) = self.selection_store.persist(persisted).await {
            self.phase = ProviderSwitchPhase::Failed {
                message: error.to_string(),
            };
            return Err(error);
        }

        self.next_generation = generation.0;
        self.active = Some(provider);
        self.phase = ProviderSwitchPhase::Ready;
        Ok(generation)
    }

    /// Returns the generation of the active provider.
    pub fn active_generation(&self) -> Option<ProviderGeneration> {
        self.active.as_ref().map(|provider| provider.generation)
    }

    /// Returns the active provider after the last successful switch.
    pub fn active(&self) -> Option<&ResolvedProvider> {
        self.active.as_ref()
    }

    /// Returns the current switch phase.
    pub fn phase(&self) -> &ProviderSwitchPhase {
        &self.phase
    }
}

/// Explicit provider route requested by runtime policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderRoute {
    /// Root conversation route.
    Root,
    /// Compaction route.
    Compaction,
    /// Tool-output summary route.
    ToolSummary,
    /// Locator route.
    Locator,
}
/// Route unavailable for an explicitly requested operation.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("provider route `{route:?}` is unavailable: {reason:?}")]
pub struct RouteUnavailable {
    /// Route that was requested.
    pub route: ProviderRoute,
    /// Why the route cannot be used.
    pub reason: RouteUnavailableReason,
}

/// Reason an explicitly requested route is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteUnavailableReason {
    /// No route exists for the requested operation.
    Missing,
    /// Route belongs to an older provider generation.
    StaleGeneration,
}

/// Returns an explicit route only when it belongs to the active generation.
pub fn require_route_for_generation(
    routes: &ProviderRoutes,
    route: ProviderRoute,
    generation: ProviderGeneration,
) -> Result<&ResolvedModelRoute, RouteUnavailable> {
    let resolved = require_route(routes, route)?;
    if resolved.generation != generation {
        return Err(RouteUnavailable {
            route,
            reason: RouteUnavailableReason::StaleGeneration,
        });
    }
    Ok(resolved)
}

/// Provider resolution failure.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Provider configuration is incomplete or inconsistent.
    #[error("provider configuration is invalid: {0}")]
    Configuration(String),
    /// Credentials cannot be resolved.
    #[error("provider credentials could not be resolved: {0}")]
    Credentials(String),
    /// A configured provider profile cannot be resolved.
    #[error("provider profile is invalid: {0}")]
    InvalidProfile(String),
    /// Provider catalog cannot be loaded.
    #[error("provider model catalog could not be loaded: {0}")]
    Catalog(String),
    /// The selected model is invalid.
    #[error("provider model selection is invalid: {0}")]
    InvalidModel(String),
    /// The selected model lacks a required capability.
    #[error("provider model capability is unavailable: {0}")]
    Capability(String),
    /// Provider transport cannot start.
    #[error("provider transport could not start: {0}")]
    Transport(String),
    /// Provider selection cannot be persisted.
    #[error("provider selection could not be persisted: {0}")]
    Persistence(String),
    /// Provider generations are exhausted.
    #[error("provider generation counter is exhausted")]
    GenerationExhausted,
}

/// Provider resolution port.
pub trait ProviderResolver: Send + Sync {
    /// Resolves a complete provider without changing the active provider.
    fn resolve(
        &self,
        selection: ModelSelection,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ResolvedProvider, ProviderError>> + Send + '_>,
    >;
}

/// Provider selection persistence port.
pub trait ProviderSelectionStore: Send + Sync {
    /// Persists an owned selection after resolution succeeds.
    fn persist(
        &self,
        selection: ProviderSelection,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ProviderError>> + Send + '_>>;

    /// Loads a previously persisted selection, if one exists.
    fn load(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<ProviderSelection>, ProviderError>> + Send + '_>>;
}

/// Returns an explicit route or a typed unavailability error.
pub fn require_route(
    routes: &ProviderRoutes,
    route: ProviderRoute,
) -> Result<&ResolvedModelRoute, RouteUnavailable> {
    match route {
        ProviderRoute::Root => Ok(&routes.root),
        ProviderRoute::Compaction => Ok(&routes.compaction),
        ProviderRoute::ToolSummary => routes.tool_summary.as_ref().ok_or(RouteUnavailable {
            route,
            reason: RouteUnavailableReason::Missing,
        }),
        ProviderRoute::Locator => routes.locator.as_ref().ok_or(RouteUnavailable {
            route,
            reason: RouteUnavailableReason::Missing,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use harness_model_api::{ContextLimits, ModelCapabilities};

    use super::*;

    struct DummyTransport;

    impl ModelTransport for DummyTransport {
        fn start(
            &self,
            _attempt: Arc<harness_model_api::ModelAttempt>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Box<dyn harness_model_api::ModelAttemptHandle>,
                            harness_model_api::ModelFailure,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            unimplemented!()
        }

        fn shutdown(
            &self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), harness_model_api::ModelFailure>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { Ok(()) })
        }
    }

    struct MockResolver {
        should_fail: bool,
        resolved_count: Arc<AtomicUsize>,
    }

    impl ProviderResolver for MockResolver {
        fn resolve(
            &self,
            selection: ModelSelection,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<ResolvedProvider, ProviderError>>
                    + Send
                    + '_,
            >,
        > {
            self.resolved_count.fetch_add(1, Ordering::SeqCst);
            let should_fail = self.should_fail;
            Box::pin(async move {
                if should_fail {
                    return Err(ProviderError::Credentials("injected failure".to_string()));
                }
                Ok(ResolvedProvider {
                    identity: ProviderIdentity {
                        id: selection.provider.clone(),
                        display_name: "Mock Provider".to_string(),
                        transport: "mock".to_string(),
                    },
                    generation: ProviderGeneration(0),
                    selected_model: selection.clone(),
                    capabilities: ModelCapabilities::default(),
                    context_limits: ContextLimits::new(1000, 800).unwrap(),
                    routes: ProviderRoutes {
                        root: ResolvedModelRoute::new(
                            selection.provider.clone(),
                            ProviderGeneration(0),
                            "root",
                            selection.clone(),
                        )
                        .unwrap(),
                        compaction: ResolvedModelRoute::new(
                            selection.provider.clone(),
                            ProviderGeneration(0),
                            "compaction",
                            selection.clone(),
                        )
                        .unwrap(),
                        tool_summary: None,
                        locator: None,
                    },
                    transport: Box::new(DummyTransport),
                })
            })
        }
    }

    struct MockSelectionStore {
        should_fail: bool,
        persist_count: Arc<AtomicUsize>,
    }

    impl ProviderSelectionStore for MockSelectionStore {
        fn persist(
            &self,
            _selection: ProviderSelection,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), ProviderError>> + Send + '_>,
        > {
            self.persist_count.fetch_add(1, Ordering::SeqCst);
            let should_fail = self.should_fail;
            Box::pin(async move {
                if should_fail {
                    return Err(ProviderError::Persistence(
                        "injected persistence failure".to_string(),
                    ));
                }
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn test_successful_switch() {
        let resolver = Arc::new(MockResolver {
            should_fail: false,
            resolved_count: Arc::new(AtomicUsize::new(0)),
        });
        let store = Arc::new(MockSelectionStore {
            should_fail: false,
            persist_count: Arc::new(AtomicUsize::new(0)),
        });

        let mut controller = ProviderController::new(resolver.clone(), store.clone());
        assert_eq!(*controller.phase(), ProviderSwitchPhase::Requested);
        assert!(controller.active().is_none());

        let selection =
            ModelSelection::new(ProviderId::new("test").unwrap(), "model-1", None, None);

        let generation = controller.switch(selection.clone()).await.unwrap();
        assert_eq!(generation, ProviderGeneration(1));
        assert_eq!(controller.active_generation(), Some(ProviderGeneration(1)));
        assert_eq!(*controller.phase(), ProviderSwitchPhase::Ready);

        let active = controller.active().unwrap();
        assert_eq!(active.generation, ProviderGeneration(1));
        assert_eq!(active.routes.root.generation, ProviderGeneration(1));
        assert_eq!(active.routes.compaction.generation, ProviderGeneration(1));

        assert_eq!(resolver.resolved_count.load(Ordering::SeqCst), 1);
        assert_eq!(store.persist_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_failed_resolve() {
        let resolver = Arc::new(MockResolver {
            should_fail: true,
            resolved_count: Arc::new(AtomicUsize::new(0)),
        });
        let store = Arc::new(MockSelectionStore {
            should_fail: false,
            persist_count: Arc::new(AtomicUsize::new(0)),
        });

        let mut controller = ProviderController::new(resolver.clone(), store.clone());

        let selection =
            ModelSelection::new(ProviderId::new("test").unwrap(), "model-1", None, None);

        let result = controller.switch(selection.clone()).await;
        assert!(result.is_err());
        assert!(controller.active().is_none());
        assert!(matches!(
            controller.phase(),
            ProviderSwitchPhase::Failed { .. }
        ));
        assert_eq!(resolver.resolved_count.load(Ordering::SeqCst), 1);
        assert_eq!(store.persist_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_failed_persist() {
        let resolver = Arc::new(MockResolver {
            should_fail: false,
            resolved_count: Arc::new(AtomicUsize::new(0)),
        });
        let store = Arc::new(MockSelectionStore {
            should_fail: true,
            persist_count: Arc::new(AtomicUsize::new(0)),
        });

        let mut controller = ProviderController::new(resolver.clone(), store.clone());

        let selection =
            ModelSelection::new(ProviderId::new("test").unwrap(), "model-1", None, None);

        let result = controller.switch(selection.clone()).await;
        assert!(result.is_err());
        assert!(controller.active().is_none());
        assert!(matches!(
            controller.phase(),
            ProviderSwitchPhase::Failed { .. }
        ));
        assert_eq!(resolver.resolved_count.load(Ordering::SeqCst), 1);
        assert_eq!(store.persist_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_routes_unavailability() {
        let selection =
            ModelSelection::new(ProviderId::new("test").unwrap(), "model-1", None, None);

        let routes = ProviderRoutes {
            root: ResolvedModelRoute::new(
                ProviderId::new("test").unwrap(),
                ProviderGeneration(2),
                "root",
                selection.clone(),
            )
            .unwrap(),
            compaction: ResolvedModelRoute::new(
                ProviderId::new("test").unwrap(),
                ProviderGeneration(2),
                "compaction",
                selection.clone(),
            )
            .unwrap(),
            tool_summary: None,
            locator: None,
        };

        // Test existing routes
        assert!(require_route(&routes, ProviderRoute::Root).is_ok());
        assert!(require_route(&routes, ProviderRoute::Compaction).is_ok());

        // Test missing routes
        let missing = require_route(&routes, ProviderRoute::ToolSummary);
        assert_eq!(
            missing,
            Err(RouteUnavailable {
                route: ProviderRoute::ToolSummary,
                reason: RouteUnavailableReason::Missing,
            })
        );

        // Test route for generation
        assert!(
            require_route_for_generation(&routes, ProviderRoute::Root, ProviderGeneration(2))
                .is_ok()
        );

        let stale =
            require_route_for_generation(&routes, ProviderRoute::Root, ProviderGeneration(1));
        assert_eq!(
            stale,
            Err(RouteUnavailable {
                route: ProviderRoute::Root,
                reason: RouteUnavailableReason::StaleGeneration,
            })
        );
    }
}

use serde::{Deserialize, Serialize};

/// Stable provider profile identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderProfileId(String);

impl ProviderProfileId {
    /// Create a provider profile identifier from a validated string.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the provider profile identifier as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// User-facing provider implementation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderImplementationKind {
    /// Codex provider using ChatGPT auth and WebSocket transport.
    Codex,
    /// Ollama Cloud provider using API-key auth and HTTPS request transport.
    OllamaCloud,
    /// HTTPS API provider using API-key auth and HTTPS request transport.
    /// TODO: rename to http not https
    HttpsApi,
}

impl ProviderImplementationKind {
    /// Return a short user-facing provider implementation kind label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OllamaCloud => "ollama-cloud",
            Self::HttpsApi => "https-api",
        }
    }
}

/// Auth configuration for a provider profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderAuthConfig {
    /// ChatGPT auth managed by the harness.
    ChatGptHarness,
    /// API key stored by the harness.
    ApiKey { credential_id: String },
}

/// Driver configuration for a provider profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderDriverConfig {
    /// Codex wire protocol over WebSocket.
    CodexWsResponses {
        /// Base URL including provider path prefix.
        base_url: String,
        /// Stream idle timeout in milliseconds.
        stream_idle_timeout_ms: u64,
    },
    /// Responses-compatible wire protocol over HTTPS.
    HttpsResponses {
        /// Base URL including provider path prefix.
        base_url: String,
        /// Request timeout in milliseconds.
        request_timeout_ms: u64,
        /// Stream idle timeout in milliseconds.
        stream_idle_timeout_ms: u64,
    },
    /// OpenAPI-compatible Chat Completions protocol over HTTP.
    #[serde(rename = "chat-completion", alias = "chat_completion")]
    ChatCompletion {
        /// Base URL including provider path prefix.
        base_url: String,
        /// Request timeout in milliseconds.
        request_timeout_ms: u64,
        /// Stream idle timeout in milliseconds.
        stream_idle_timeout_ms: u64,
    },
}

impl ProviderDriverConfig {
    /// Return the configured base URL.
    pub fn base_url(&self) -> &str {
        match self {
            Self::CodexWsResponses { base_url, .. }
            | Self::HttpsResponses { base_url, .. }
            | Self::ChatCompletion { base_url, .. } => base_url,
        }
    }

    /// Return the stream idle timeout in milliseconds.
    pub fn stream_idle_timeout_ms(&self) -> u64 {
        match self {
            Self::CodexWsResponses {
                stream_idle_timeout_ms,
                ..
            }
            | Self::HttpsResponses {
                stream_idle_timeout_ms,
                ..
            }
            | Self::ChatCompletion {
                stream_idle_timeout_ms,
                ..
            } => *stream_idle_timeout_ms,
        }
    }
}

/// Level of support for `developer` role in place of `system`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeveloperRoleSupport {
    /// Disallow developer role / use standard `system` role.
    #[default]
    Disabled,
    /// Support developer role (maps `system` messages to `developer`).
    Supported,
    /// Only allow developer role and disallow `system` role.
    DeveloperOnly,
}

/// Locally configured model capability metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelConfig {
    /// Unique slug of the model.
    pub slug: String,
    /// Context window size in tokens.
    pub context_window: u64,
    /// Percent of context window that is effective before compaction.
    pub effective_context_window_percent: u64,
    /// Whether the model accepts tool definitions.
    pub supports_tools: bool,
    /// Whether the model supports parallel tool calls.
    pub supports_parallel_tool_calls: bool,
    /// List of available service tiers.
    #[serde(default)]
    pub service_tiers: Vec<String>,
    /// Developer role support override for this model (inherits provider default if None).
    #[serde(default)]
    pub developer_role_support: Option<DeveloperRoleSupport>,
    /// Whether the model allows multiple system/developer messages (inherits provider default if None).
    #[serde(default)]
    pub allow_multiple_system_messages: Option<bool>,
    /// Whether response storing is enabled (`store: true` vs `store: false`) (inherits provider default if None).
    #[serde(default)]
    pub store: Option<bool>,
}

/// Provider profile persisted by the harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Stable profile identifier.
    pub id: ProviderProfileId,
    /// User-facing provider display name.
    pub display_name: String,
    /// User-facing provider kind.
    pub kind: ProviderImplementationKind,
    /// Auth configuration.
    pub auth: Option<ProviderAuthConfig>,
    /// Driver configuration.
    pub driver: ProviderDriverConfig,
    /// Default model slug for new sessions.
    pub default_model: String,
    /// Default reasoning effort.
    pub default_reasoning_effort: Option<String>,
    /// Default service tier.
    pub default_service_tier: Option<String>,
    /// Locally configured model capability metadata.
    #[serde(default)]
    pub model_configs: Vec<ProviderModelConfig>,
    /// Model used for tool-output summary requests.
    pub tool_output_summary_model: Option<String>,
    /// Provider-wide default level of support for `developer` role.
    #[serde(default)]
    pub developer_role_support: DeveloperRoleSupport,
    /// Provider-wide default for whether multiple system/developer messages are allowed.
    #[serde(default = "default_true")]
    pub allow_multiple_system_messages: bool,
    /// Provider-wide default for whether responses should be stored (`store: true`).
    #[serde(default = "default_true")]
    pub store: bool,
}

fn default_true() -> bool {
    true
}

/// Provider configuration document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Default provider profile id.
    pub default_profile_id: ProviderProfileId,
    /// Configured provider profiles.
    pub profiles: Vec<ProviderProfile>,
}

impl ProviderConfig {
    /// Return the default provider profile.
    pub fn default_profile(&self) -> Option<&ProviderProfile> {
        self.profile(&self.default_profile_id)
    }

    /// Return a provider profile by id.
    pub fn profile(&self, id: &ProviderProfileId) -> Option<&ProviderProfile> {
        self.profiles.iter().find(|profile| &profile.id == id)
    }
}

/// Default Codex WebSocket endpoint URL.
pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
