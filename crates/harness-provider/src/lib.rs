//! Provider resolution and switching contracts.
//!
//! This crate owns provider-neutral resolution policy. Credential loading,
//! model catalogs, and network transports are injected through ports.

use harness_model_api::{ContextLimits, ModelCapabilities, ModelSelection, ModelTransport, ProviderId, ResolvedModelRoute};

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
    /// Credentials cannot be resolved.
    #[error("provider credentials could not be resolved: {0}")]
    Credentials(String),
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
        Box<
            dyn std::future::Future<Output = Result<ResolvedProvider, ProviderError>>
                + Send
                + '_,
        >,
    >;
}

/// Provider selection persistence port.
pub trait ProviderSelectionStore: Send + Sync {
    /// Persists an owned selection after resolution succeeds.
    fn persist(
        &self,
        selection: ProviderSelection,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), ProviderError>> + Send + '_>,
    >;
}

/// Returns an explicit route or a typed unavailability error.
pub fn require_route(
    routes: &ProviderRoutes,
    route: ProviderRoute,
) -> Result<&ResolvedModelRoute, RouteUnavailable> {
    match route {
        ProviderRoute::Root => Ok(&routes.root),
        ProviderRoute::Compaction => Ok(&routes.compaction),
        ProviderRoute::ToolSummary => routes
            .tool_summary
            .as_ref()
            .ok_or(RouteUnavailable {
                route,
                reason: RouteUnavailableReason::Missing,
            }),
        ProviderRoute::Locator => routes
            .locator
            .as_ref()
            .ok_or(RouteUnavailable {
                route,
                reason: RouteUnavailableReason::Missing,
            }),
    }
}
