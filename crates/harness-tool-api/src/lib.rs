//! Provider-independent contracts for tool discovery, invocation, and results.

use std::{collections::BTreeMap, sync::Arc};

use thiserror::Error;
use std::sync::RwLock;

/// Stable name assigned to a tool.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolName(String);

impl ToolName {
    /// Creates a tool name from a nonempty value.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidToolName> {
        let value = value.into();
        if value.is_empty() {
            return Err(InvalidToolName);
        }
        Ok(Self(value))
    }

    /// Returns the tool name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ToolName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Failure returned when a tool name is empty.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("tool name is empty")]
pub struct InvalidToolName;

/// Syntax used by a freeform tool input grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrammarSyntax {
    /// A grammar represented by a regular expression.
    Regex,
    /// A grammar represented by Lark context-free grammar syntax.
    Lark,
}

/// JSON schema used to describe function arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonSchema(String);

impl JsonSchema {
    /// Creates a JSON schema from its serialized representation.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the serialized JSON schema.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Schema accepted by a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolInputSchema {
    /// A provider-independent freeform grammar.
    FreeformGrammar {
        /// Grammar syntax.
        syntax: GrammarSyntax,
        /// Grammar definition.
        definition: String,
    },
    /// A JSON schema for function arguments.
    JsonSchema(JsonSchema),
}

/// Input supplied to a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolInput {
    /// Freeform input.
    Freeform(String),
    /// Serialized function arguments.
    FunctionJson(String),
}
impl ToolInput {
    /// Returns the exact model-provided input representation.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Freeform(input) | Self::FunctionJson(input) => input,
        }
    }
}

/// Identifier that correlates a tool call with its result.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Creates a nonempty tool call identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, EmptyToolCallId> {
        let value = value.into();
        if value.is_empty() {
            return Err(EmptyToolCallId);
        }
        Ok(Self(value))
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Failure returned by tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolFailure {
    /// The tool rejects its input.
    InvalidInput(String),
    /// The tool cannot complete before its deadline.
    TimedOut,
    /// The tool is cancelled before completion.
    Cancelled,
    /// The tool fails without a more specific category.
    Execution(String),
}

/// Presentation-specific tool result information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPresentation {
    /// Short label displayed by a frontend.
    pub label: String,
    /// Optional structured display payload.
    pub display: Option<String>,
}

/// Artifact produced by tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolArtifact {
    /// Artifact name.
    pub name: String,
    /// Artifact content or locator.
    pub value: String,
}

/// Result returned by tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// Output supplied to the model.
    pub model_output: String,
    /// Optional frontend presentation.
    pub presentation: Option<ToolPresentation>,
    /// Artifacts and metadata produced by the tool.
    pub artifacts: Vec<ToolArtifact>,
}

/// Capabilities exposed by a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolCapabilities {
    /// The tool reads workspace state without changing it.
    pub read_only: bool,
    /// The tool can change workspace state.
    pub mutates_workspace: bool,
    /// The tool can safely retry after an unresolved restart.
    pub idempotent: bool,
}

/// Provider-independent tool definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    /// Stable tool name.
    pub name: ToolName,
    /// Human-readable description.
    pub description: String,
    /// Input schema.
    pub input_schema: ToolInputSchema,
    /// Execution capabilities.
    pub capabilities: ToolCapabilities,
}

/// Native tool specification constructed by a tool module.
///
/// The native construction API accepts Lark grammars. JSON Schema remains part
/// of [`ToolDefinition`] for provider compatibility, but native tool modules do
/// not construct their definitions through a JSON Schema builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    definition: ToolDefinition,
    executor: ToolExecutorRoute,
}

impl ToolSpec {
    /// Creates a native tool specification whose default route is its name.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidToolName> {
        let name = ToolName::new(name)?;
        let executor = ToolExecutorRoute {
            identifier: name.as_str().to_owned(),
        };
        Ok(Self {
            definition: ToolDefinition {
                name,
                description: String::new(),
                input_schema: ToolInputSchema::FreeformGrammar {
                    syntax: GrammarSyntax::Lark,
                    definition: String::new(),
                },
                capabilities: ToolCapabilities::default(),
            },
            executor,
        })
    }

    /// Sets the model-facing description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.definition.description = description.into();
        self
    }

    /// Sets the native Lark grammar loaded by the tool module.
    pub fn lark(mut self, definition: impl Into<String>) -> Self {
        self.definition.input_schema = ToolInputSchema::FreeformGrammar {
            syntax: GrammarSyntax::Lark,
            definition: definition.into(),
        };
        self
    }

    /// Sets the tool execution capabilities.
    pub fn capabilities(mut self, capabilities: ToolCapabilities) -> Self {
        self.definition.capabilities = capabilities;
        self
    }

    /// Overrides the executor route assigned to this tool.
    pub fn route(mut self, route: impl Into<String>) -> Result<Self, EmptyExecutorRoute> {
        self.executor = ToolExecutorRoute::new(route)?;
        Ok(self)
    }

    /// Converts this construction specification into a provider registration.
    pub fn build(self) -> AdvertisedTool {
        AdvertisedTool {
            definition: self.definition,
            executor: self.executor,
        }
    }
}

/// Executor route assigned to an advertised tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutorRoute {
    /// Stable route identifier resolved by the composition root.
    pub identifier: String,
}

impl ToolExecutorRoute {
    /// Creates an executor route from a nonempty identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, EmptyExecutorRoute> {
        let value = value.into();
        if value.is_empty() {
            return Err(EmptyExecutorRoute);
        }
        Ok(Self { identifier: value })
    }
}

/// Tool definition together with its resolved executor route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisedTool {
    /// Provider-independent definition.
    pub definition: ToolDefinition,
    /// Executor route selected by composition.
    pub executor: ToolExecutorRoute,
}

/// Shared availability policy for dynamically enabling or disabling tools.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolAvailability {
    rules: Vec<(String, bool)>,
}

impl ToolAvailability {
    /// Creates a policy with every registered tool enabled by default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the enabled state for a glob pattern such as `terminal*`.
    pub fn set(&mut self, pattern: impl Into<String>, enabled: bool) -> Result<(), ToolPatternError> {
        let pattern = pattern.into();
        if pattern.is_empty() || pattern == "*" && !enabled {
            return Err(ToolPatternError);
        }
        self.rules.push((pattern, enabled));
        Ok(())
    }

    /// Returns whether a tool is enabled. The most recently matching rule wins.
    pub fn is_enabled(&self, tool: &str) -> bool {
        self.rules
            .iter()
            .rev()
            .find(|(pattern, _)| glob_matches(pattern, tool))
            .map(|(_, enabled)| *enabled)
            .unwrap_or(true)
    }

    /// Returns the configured rules in insertion order.
    pub fn rules(&self) -> &[(String, bool)] {
        &self.rules
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let mut states = vec![false; value.len() + 1];
    states[0] = true;
    for symbol in pattern {
        let mut next = vec![false; value.len() + 1];
        if symbol == '*' {
            let mut seen = false;
            for index in 0..=value.len() {
                seen |= states[index];
                next[index] = seen;
            }
        } else {
            for index in 0..value.len() {
                if states[index] && value[index] == symbol {
                    next[index + 1] = true;
                }
            }
        }
        states = next;
    }
    states[value.len()]
}

/// Failure returned when a tool availability pattern is invalid.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("tool availability pattern is invalid")]
pub struct ToolPatternError;

/// Executor wrapper that applies live availability to already-built requests.
pub struct AvailabilityToolExecutor {
    inner: Arc<dyn ToolExecutor>,
    availability: Arc<RwLock<ToolAvailability>>,
}

impl AvailabilityToolExecutor {
    /// Wraps an executor with shared dynamic availability state.
    pub fn new(
        inner: Arc<dyn ToolExecutor>,
        availability: Arc<RwLock<ToolAvailability>>,
    ) -> Self {
        Self { inner, availability }
    }
}

impl ToolExecutor for AvailabilityToolExecutor {
    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> {
        let enabled = self
            .availability
            .read()
            .map(|policy| policy.is_enabled(request.tool.as_str()))
            .unwrap_or(false);
        if !enabled {
            return Box::pin(async {
                Ok(ToolResult {
                    model_output: "tool is unavailable.".to_owned(),
                    presentation: None,
                    artifacts: Vec::new(),
                })
            });
        }
        self.inner.execute(request)
    }
}

/// Registered tools with unique names and executable routes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolRegistry {
    tools: BTreeMap<ToolName, AdvertisedTool>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a builder for native tool specifications.
    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::default()
    }

    /// Registers a tool and rejects duplicate names.
    pub fn register(&mut self, tool: AdvertisedTool) -> Result<(), DuplicateToolName> {
        let name = tool.definition.name.clone();
        if self.tools.contains_key(&name) {
            return Err(DuplicateToolName(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Returns a registered tool by name.
    pub fn get(&self, name: &ToolName) -> Option<&AdvertisedTool> {
        self.tools.get(name)
    }

    /// Returns the number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Returns whether the registry has no tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Iterates over tools in stable name order.
    pub fn iter(&self) -> impl Iterator<Item = (&ToolName, &AdvertisedTool)> {
        self.tools.iter()
    }
}

/// Builder for a provider-facing registry of native tool specifications.
#[derive(Debug, Default)]
pub struct ToolRegistryBuilder {
    tools: Vec<ToolSpec>,
}

impl ToolRegistryBuilder {
    /// Adds one native tool specification.
    pub fn tool(mut self, tool: ToolSpec) -> Self {
        self.tools.push(tool);
        self
    }

    /// Builds the registry and rejects duplicate names.
    pub fn build(self) -> Result<ToolRegistry, DuplicateToolName> {
        let mut registry = ToolRegistry::new();
        for tool in self.tools {
            registry.register(tool.build())?;
        }
        Ok(registry)
    }
}

/// Stable identifier for one tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolExecutionId(pub u64);

/// Cancellation and deadline policy for one tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolExecutionPolicy {
    /// Maximum execution duration in milliseconds.
    pub deadline_ms: u64,
    /// Whether cancellation is accepted after execution starts.
    pub cancellable: bool,
}

/// Input passed to an executor route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionRequest {
    /// Execution identity.
    pub execution_id: ToolExecutionId,
    /// Advertised tool name.
    pub tool: ToolName,
    /// Executor route selected when the tool was registered.
    pub route: ToolExecutorRoute,
    /// Tool input.
    pub input: ToolInput,
    /// Execution policy.
    pub policy: ToolExecutionPolicy,
}

/// Provider-independent tool execution port.
pub trait ToolExecutor: Send + Sync {
    /// Executes one tool request under its explicit policy.
    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>,
    >;
}
/// Failure returned when a tool call identifier is empty.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("tool call identifier is empty")]
pub struct EmptyToolCallId;

/// Failure returned when an executor route identifier is empty.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("tool executor route identifier is empty")]
pub struct EmptyExecutorRoute;

/// Failure returned when a registry already contains a tool name.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("tool {0:?} is already registered")]
pub struct DuplicateToolName(pub ToolName);

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> AdvertisedTool {
        AdvertisedTool {
            definition: ToolDefinition {
                name: ToolName::new(name).unwrap(),
                description: String::from("test"),
                input_schema: ToolInputSchema::JsonSchema(JsonSchema::new("{}")),
                capabilities: ToolCapabilities::default(),
            },
            executor: ToolExecutorRoute::new(format!("{name}-executor")).unwrap(),
        }
    }

    #[test]
    fn registry_rejects_duplicate_names_without_replacing_existing_route() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("inspect")).unwrap();
        let duplicate = tool("inspect");

        assert_eq!(
            registry.register(duplicate),
            Err(DuplicateToolName(ToolName::new("inspect").unwrap()))
        );
        assert_eq!(
            registry
                .get(&ToolName::new("inspect").unwrap())
                .unwrap()
                .executor
                .identifier,
            "inspect-executor"
        );
    }
}
